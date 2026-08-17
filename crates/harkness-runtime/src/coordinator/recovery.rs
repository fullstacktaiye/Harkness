//! The startup sweep: finding runs whose process is gone and ending them.
//!
//! A Harkness process that dies leaves its runs exactly as they were — `queued`,
//! `running`, or `waiting_for_approval` — with no worker left to move them.
//! Without this pass they stay that way for good, indistinguishable from live
//! work: a front end shows a spinner nothing is behind, a retry is refused
//! because the run "is still active", and the history #82 and #83 build on
//! quietly stops being true.
//!
//! The sweep runs once, at coordinator construction, before any new work is
//! accepted. It claims *only* runs whose owning claim is provably dead, so a
//! second Harkness sharing the data directory — the command line beside a
//! running application — is never disturbed.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use time::OffsetDateTime;

use crate::approval::ApprovalGate;
use crate::domain::{ApprovalId, LeaseId, RunId};
use crate::store::{LeaseRecord, Store};

use super::RuntimeError;
use super::lease::{self, Liveness, RecoveryLock, RuntimeLease};

/// What one startup sweep found and did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    interrupted_runs: Vec<RunId>,
    expired_approvals: Vec<ApprovalId>,
    failures: Vec<RecoveryFailure>,
    contended: bool,
}

impl RecoveryReport {
    /// Runs this sweep marked `interrupted`, oldest first.
    #[must_use]
    pub fn interrupted_runs(&self) -> &[RunId] {
        &self.interrupted_runs
    }

    /// Approval requests this sweep resolved without an answer.
    ///
    /// Every one of them is `superseded`, which is terminal, so a prompt still
    /// open in a restarted front end can no longer authorize anything.
    #[must_use]
    pub fn expired_approvals(&self) -> &[ApprovalId] {
        &self.expired_approvals
    }

    /// Runs that could not be recovered, with the reason each one failed.
    ///
    /// Reported rather than retried or silently skipped: one poisoned record
    /// must not stop the other ninety-nine being recovered, and it must not
    /// disappear either.
    #[must_use]
    pub fn failures(&self) -> &[RecoveryFailure] {
        &self.failures
    }

    /// Whether another process was already sweeping and did not finish in time.
    ///
    /// A contended report is empty rather than wrong: the sweep that held the
    /// lock covers the same candidates this one would have.
    #[must_use]
    pub const fn was_contended(&self) -> bool {
        self.contended
    }

    /// Whether the sweep found nothing to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.interrupted_runs.is_empty() && self.failures.is_empty()
    }
}

/// One run the sweep could not mark, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryFailure {
    run: RunId,
    kind: &'static str,
    message: String,
}

impl RecoveryFailure {
    /// The run that stayed non-terminal.
    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }

    /// Stable machine-readable discriminant of the failure.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    /// Human-readable detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Marks every run whose owning claim is provably dead.
///
/// `own` is this coordinator's own lease, and it is excluded by identity rather
/// than by probing: this process holds that lock, so a probe would answer
/// "held" anyway, and skipping it keeps the sweep honest about only ever
/// claiming runs it can prove nobody is driving.
pub(super) fn sweep(
    store: &Store,
    approvals: &ApprovalGate,
    own: &RuntimeLease,
    at: OffsetDateTime,
) -> Result<RecoveryReport, RuntimeError> {
    let data_dir = store.data_dir().to_path_buf();
    let Some(_lock) = RecoveryLock::acquire(&data_dir)? else {
        return Ok(RecoveryReport {
            contended: true,
            ..RecoveryReport::default()
        });
    };

    let mut report = RecoveryReport::default();
    // A claim is read and probed once, however many runs point at it. That is
    // partly cost — a process that died holding ten runs is one filesystem
    // question rather than ten — and partly consistency: marking the first run
    // writes the claim off, so re-reading it would report a different reason
    // for every run of one death.
    let mut examined: HashMap<LeaseId, (Option<LeaseRecord>, Liveness)> = HashMap::new();

    for (run, owner) in store.unfinished_runs()? {
        if owner == Some(own.id()) {
            continue;
        }
        let (record, liveness) = match owner {
            Some(id) => match examined.entry(id) {
                Entry::Occupied(examined) => examined.get().clone(),
                // Not `or_insert_with`: reading the row can fail, and a closure
                // has nowhere to put that failure but a panic.
                Entry::Vacant(slot) => slot
                    .insert((store.lease(id)?, lease::probe(&data_dir, id)))
                    .clone(),
            },
            None => (None, Liveness::Released),
        };
        let Some(reason) = lease::interruption_reason(record.as_ref(), &liveness, at) else {
            continue;
        };
        match store.interrupt_run(run, reason, at) {
            Ok(Some(interruption)) => {
                report.interrupted_runs.push(interruption.run());
                report.expired_approvals.extend(interruption.approval_ids());
                // Anything parked on one of these questions is released with a
                // resolution it can act on rather than left waiting for an
                // answer nobody can give. A request with no live waiter is
                // discarded by the gate, which is every request here in the
                // ordinary restart case.
                for request in interruption.approvals() {
                    approvals.resolve_from(request);
                }
            }
            // Already terminal: the run's own process, or a sweep that raced
            // this one, got there first. Nothing to report and nothing wrong.
            Ok(None) => {}
            Err(error) => report.failures.push(RecoveryFailure {
                run,
                kind: error.kind(),
                message: error.to_string(),
            }),
        }
    }

    // A dead claim whose runs are now all terminal keeps neither its lock file
    // nor a live-looking row: `interrupt_run` released the row inside the same
    // transaction, and the file is removed here because nothing will ever open
    // that path again.
    for (id, (_, liveness)) in examined {
        if matches!(liveness, Liveness::Released) {
            let _ = store.release_lease(id, at);
            lease::discard(&data_dir, id);
        }
    }

    Ok(report)
}
