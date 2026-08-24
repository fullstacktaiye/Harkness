//! The single orchestration loop shared by every Harkness front end.
//!
//! A run owns one worker thread and one agent. Every requested tool follows the
//! same route: schema-valid preparation, policy, optional durable approval,
//! scheduler admission, and executor delivery. The approval path is:
//!
//! ```text
//! request -> take gate ticket -> persist request -> mark run waiting
//!         -> park with no store transaction or scheduler slot held
//!         -> persisted decision -> gate wake -> re-check binding -> schedule
//! ```
//!
//! # Interruption is an outcome, not a gap
//!
//! A coordinator holds one lease: an advisory lock file the kernel
//! releases when this process dies, plus a row every run it starts points at.
//! Construction sweeps the store first, and every run whose claim is provably
//! dead is marked `interrupted` — the run, its unfinished steps, its in-flight
//! calls, and the questions nobody can answer any more — with the timeline
//! before that moment left exactly as the dead process wrote it. A live
//! sibling's runs are never touched, because the proof is the lock rather than
//! a timestamp.
//!
//! Nothing is resumed. [`RunCoordinator::retry_run`] starts a *new* run for the
//! same task, recording which attempt it follows and whether that attempt may
//! already have changed the workspace; the original keeps its own history, and
//! no approval it was granted carries over.

mod error;
mod lease;
mod recovery;
mod snapshot;
mod subscription;
#[cfg(test)]
mod tests;

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread;

use harkness_git::Cancellation;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::agent::{
    Agent, AgentAction, AgentActionRecord, Observation, ObservationRecord, TaskRef, ToolErrorView,
    ToolResultView, WorkspaceRef,
};
use crate::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, ApprovalScope, ApprovalState, ApprovalVerdict,
    CandidateCall, PendingApproval, WorkspaceBinding, canonical_input_hash, grant_applies,
    matching_grants,
};
use crate::domain::{
    ExecutionState, Failure, Run, RunId, Step, StepId, Task, TaskId, ToolCall, ToolCallState,
};
use crate::observe;
use crate::policy::{PolicyEngine, PolicyRequest, PolicyVerdict};
use crate::schedule::{ScheduledCall, Scheduler, WorkspaceKey};
use crate::store::{
    DEFAULT_EVENT_PAGE_LIMIT, EventKind, EventPage, EventSeq, RunEvent, RunPage, Store, StoredEvent,
};
use crate::tool::{
    CallOutcome, ExecutionError, MAX_FAILURE_MESSAGE_BYTES, ToolExecutor, ToolRegistry,
    WorkspaceMetadata, truncate_failure_text,
};
use crate::trust::{ExecutionMode, PathBoundary};

use lease::RuntimeLease;

const MAX_AGENT_TURNS: usize = 1_024;
const WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

/// How long a dropped coordinator waits for its own workers to stop.
///
/// A run still executing at exit is cancelled cooperatively and then let go: the
/// lease is released either way, so what this bounds is how long an exiting
/// process is willing to wait before the next start finds those runs
/// `interrupted` instead.
const SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

struct ActiveRun {
    cancellation: Cancellation,
}

#[derive(Default)]
struct DeliveryState {
    cursor: Option<EventSeq>,
    subscribers: Vec<Weak<subscription::Subscriber>>,
    closed: bool,
}

#[derive(Default)]
struct RunDelivery {
    state: Mutex<DeliveryState>,
}

struct CoordinatorInner {
    store: Arc<Store>,
    registry: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    approvals: Arc<ApprovalGate>,
    scheduler: Arc<Scheduler>,
    active: Mutex<HashMap<RunId, ActiveRun>>,
    deliveries: Mutex<HashMap<RunId, Arc<RunDelivery>>>,
    /// This process's claim on the runs it starts. Dropping it releases the
    /// advisory lock, which is what the next start reads as "abandoned".
    lease: RuntimeLease,
    /// Trips once, on the first shutdown; a second is a no-op.
    ///
    /// Read under [`claim`](Self::claim) by anything that is about to record a
    /// run, never on its own: the flag alone is a moment in the past by the
    /// time the row is written.
    stopping: AtomicBool,
    /// Held shared while a run is being recorded and started, exclusively while
    /// the claim is given up.
    ///
    /// Reading `stopping` and then inserting a run is two steps, and shutdown
    /// fits between them: the row would land naming a lease whose row was
    /// already released and whose lock file was already unlinked, so the next
    /// sweep would interrupt a run this process is actively driving — and a
    /// user could then retry it while the original worker still holds the
    /// worktree. That is the outcome ADR-0020 exists to prevent, so the check
    /// and the write it guards happen under one guard.
    claim: std::sync::RwLock<()>,
    /// Woken by shutdown so housekeeping exits at once rather than at the end
    /// of its renewal interval.
    housekeeping: Condvar,
    housekeeping_state: Mutex<()>,
}

/// Shared application service that owns every run's orchestration loop.
#[derive(Clone)]
pub struct RunCoordinator {
    inner: Arc<CoordinatorInner>,
    /// Shuts the coordinator down when the last *handle* is dropped.
    ///
    /// It cannot live on [`CoordinatorInner`], and the reason is the case that
    /// matters most: a run worker holds an `Arc` to the inner for as long as it
    /// is driving, and a worker parked on an approval nobody will ever answer
    /// holds one for ever. Hanging teardown off the inner would therefore mean
    /// the coordinator is torn down exactly when there is nothing to tear down.
    /// Workers never hold this, so the last handle going away is what trips it,
    /// and the shutdown it runs is what releases those workers.
    _shutdown: Arc<ShutdownGuard>,
}

struct ShutdownGuard {
    inner: Weak<CoordinatorInner>,
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.shutdown();
        }
    }
}

impl RunCoordinator {
    /// Builds a coordinator with the production executor and scheduler.
    ///
    /// Takes this process's run claim and sweeps the store before returning, so
    /// no new work is accepted while runs abandoned by a dead process still
    /// look live. See [`RunCoordinator::open`] for the sweep's own report.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LeaseUnavailable`] when the claim cannot be
    /// taken, and [`RuntimeError::Store`] when the sweep cannot read the store.
    pub fn new(
        store: Arc<Store>,
        registry: Arc<ToolRegistry>,
        policy: PolicyEngine,
    ) -> Result<Self, RuntimeError> {
        let executor = ToolExecutor::new(Arc::clone(&store), Arc::clone(&registry));
        Self::with_scheduler(
            store,
            registry,
            Arc::new(policy),
            Arc::new(ApprovalGate::new()),
            Arc::new(Scheduler::new(executor)),
        )
    }

    /// Builds a coordinator from explicitly shared runtime services.
    ///
    /// # Errors
    ///
    /// As [`RunCoordinator::new`].
    pub fn with_scheduler(
        store: Arc<Store>,
        registry: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        approvals: Arc<ApprovalGate>,
        scheduler: Arc<Scheduler>,
    ) -> Result<Self, RuntimeError> {
        Self::open(store, registry, policy, approvals, scheduler)
            .map(|(coordinator, _)| coordinator)
    }

    /// Builds a coordinator and returns what its startup sweep found.
    ///
    /// The sweep is not optional and there is deliberately no constructor that
    /// skips it: a process that started accepting work while an abandoned run
    /// still read as `running` would be one that never notices, since detection
    /// happens at startup and nowhere else. What is optional is *caring* — a
    /// front end that wants to tell the user "three runs were interrupted while
    /// Harkness was not running" uses this, and everything else uses
    /// [`with_scheduler`](Self::with_scheduler) and ignores the report.
    ///
    /// # Errors
    ///
    /// As [`RunCoordinator::new`].
    pub fn open(
        store: Arc<Store>,
        registry: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        approvals: Arc<ApprovalGate>,
        scheduler: Arc<Scheduler>,
    ) -> Result<(Self, RecoveryReport), RuntimeError> {
        let now = OffsetDateTime::now_utc();
        let lease = RuntimeLease::acquire(store.data_dir(), now)?;
        let report = recovery::sweep(&store, &approvals, &lease, now)?;
        // Counts rather than a list of identifiers: a sweep after a crash can
        // touch hundreds of runs, and the per-run detail is already in each
        // run's own timeline. What a log is for here is noticing that a sweep
        // happened at all.
        tracing::info!(
            lease_id = %lease.id(),
            interrupted_runs = report.interrupted_runs().len(),
            expired_approvals = report.expired_approvals().len(),
            failures = report.failures().len(),
            contended = report.was_contended(),
            "recovery sweep complete"
        );
        let inner = Arc::new(CoordinatorInner {
            store,
            registry,
            policy,
            approvals,
            scheduler,
            active: Mutex::new(HashMap::new()),
            deliveries: Mutex::new(HashMap::new()),
            lease,
            stopping: AtomicBool::new(false),
            claim: std::sync::RwLock::new(()),
            housekeeping: Condvar::new(),
            housekeeping_state: Mutex::new(()),
        });
        let coordinator = Self {
            _shutdown: Arc::new(ShutdownGuard {
                inner: Arc::downgrade(&inner),
            }),
            inner,
        };
        coordinator.start_housekeeping();
        Ok((coordinator, report))
    }

    /// Runs the lease-renewal loop on a thread that cannot keep this alive.
    ///
    /// It holds a [`Weak`], so the coordinator is dropped exactly when its last
    /// clone goes away rather than when this thread next wakes; the thread then
    /// observes the dead pointer and exits.
    fn start_housekeeping(&self) {
        let inner = Arc::downgrade(&self.inner);
        let spawned = thread::Builder::new()
            .name("harkness-run-lease".to_owned())
            .spawn(move || {
                loop {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    if inner.stopping.load(Ordering::Acquire) {
                        return;
                    }
                    // Renewing can only widen the window in which this claim is
                    // treated as alive, so a failure is a diagnostic rather than
                    // a reason to stop: the lock file is what proves liveness.
                    let _ = inner
                        .store
                        .renew_lease(inner.lease.id(), OffsetDateTime::now_utc());
                    let guard = inner
                        .housekeeping_state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    // Re-read *under* the mutex. Checking only at the top of the
                    // loop leaves a window in which shutdown sets the flag and
                    // signals before this thread parks, and the wake-up is then
                    // lost for a whole renewal interval.
                    if inner.stopping.load(Ordering::Acquire) {
                        return;
                    }
                    // This thread necessarily holds a strong reference while it
                    // waits — the mutex and the condition variable live inside
                    // it — so a lost wake-up would keep the store and the
                    // scheduler alive, with their files open, for a further
                    // interval after the coordinator's last handle went away.
                    // That is the second thing the re-read above buys.
                    let (_guard, _timeout) = inner
                        .housekeeping
                        .wait_timeout(guard, lease::LEASE_RENEW_INTERVAL)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            });
        // A coordinator with no housekeeping thread is still correct: it simply
        // stops refreshing a timestamp that only ever widens its own survival
        // window, and its lock file goes on proving liveness by itself.
        drop(spawned);
    }

    /// Stops accepting work, cancels what is in flight, and gives up the claim.
    ///
    /// Idempotent, and run by [`Drop`] as well, so a process that exits without
    /// calling it still leaves its runs findable rather than silently lost: the
    /// claim is released, and whatever was still executing is marked
    /// `interrupted` by the next start.
    ///
    /// The wait is bounded, at thirty seconds. A run that outlives it is
    /// not abandoned quietly — it is exactly the run the next sweep ends.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }

    /// What this coordinator's claim on its runs is recorded as.
    #[must_use]
    pub fn lease_id(&self) -> crate::domain::LeaseId {
        self.inner.lease.id()
    }

    /// The run store this coordinator records into.
    ///
    /// A caller that has to build a projection out of the same records — or
    /// redact a value the way this coordinator will — needs the store that
    /// belongs to the coordinator rather than one it opened separately, so that
    /// the two cannot be different stores.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// Persists a user task and returns its stable identity.
    pub fn start_task(&self, task: Task) -> Result<TaskId, RuntimeError> {
        let id = task.id();
        self.inner.store.insert_task(&task)?;
        Ok(id)
    }

    /// Starts one run on its own worker and returns immediately.
    pub fn start_run(
        &self,
        task_id: TaskId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
    ) -> Result<RunId, RuntimeError> {
        self.start_run_inner(task_id, agent, workspace, None, None)
    }

    /// Starts a run with authoritative project catalog metadata available to
    /// every tool invocation.
    pub fn start_run_with_workspace_metadata(
        &self,
        task_id: TaskId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
        metadata: WorkspaceMetadata,
    ) -> Result<RunId, RuntimeError> {
        self.start_run_inner(task_id, agent, workspace, Some(metadata), None)
    }

    /// Starts a fresh attempt at the task `run` was an attempt at.
    ///
    /// A retry is a new run and never a continuation. The original's timeline is
    /// left exactly as it stands and gains one `run_retried` entry naming its
    /// successor; the new run records which attempt it follows, so provenance
    /// reads in both directions without either record being rewritten.
    ///
    /// # What "safe to retry" means
    ///
    /// It does not mean the workspace is as the task first found it. Nothing in
    /// v0.3 rolls back or re-applies a partial mutation, so if the earlier
    /// attempt started any tool call that could write, the new run carries
    /// [`Run::workspace_may_be_modified`] and a front end must say so. The flag
    /// is computed from persisted lifecycle — a call that entered `running` —
    /// and never from whether the tool "probably" finished.
    ///
    /// No approval carries over. Grants are matched on the run they were given
    /// for, so every protected call in the new run is evaluated and answered
    /// again from scratch.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::RunStillActive`] when the run's record has not
    /// reached a terminal state — which is also how a run another process is
    /// driving stays refused — [`RuntimeError::RunNotRetryable`] when it
    /// succeeded and there is nothing to re-attempt, and
    /// [`RuntimeError::WorkspaceUnavailable`] when the task's workspace no
    /// longer resolves.
    pub fn retry_run(
        &self,
        run: RunId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
    ) -> Result<RunId, RuntimeError> {
        self.retry_run_inner(run, agent, workspace, None)
    }

    /// Retries a run with authoritative project catalog metadata.
    ///
    /// # Errors
    ///
    /// As [`RunCoordinator::retry_run`].
    pub fn retry_run_with_workspace_metadata(
        &self,
        run: RunId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
        metadata: WorkspaceMetadata,
    ) -> Result<RunId, RuntimeError> {
        self.retry_run_inner(run, agent, workspace, Some(metadata))
    }

    fn retry_run_inner(
        &self,
        original: RunId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
        workspace_metadata: Option<WorkspaceMetadata>,
    ) -> Result<RunId, RuntimeError> {
        let record = self.inner.store.load_run(original)?;
        // The persisted state is the whole test, deliberately, and a live
        // worker in *this* process is not consulted. A run that has recorded a
        // terminal state has finished every tool body it started; what is left
        // of its worker is bookkeeping that touches no workspace and no new
        // run. Refusing during that window would make a retry offered the
        // instant a run shows `failed` succeed or fail by timing.
        //
        // The check still covers the case it has to. A run another process is
        // driving has a non-terminal record, and stays refused until that
        // process finishes it or a sweep ends it.
        if !record.state().is_terminal() {
            return Err(RuntimeError::RunStillActive { run: original });
        }
        if record.state() == ExecutionState::Succeeded {
            return Err(RuntimeError::RunNotRetryable {
                run: original,
                state: record.state(),
            });
        }
        // Refused on recorded state, like the check above, and for the same
        // reason: two live attempts at one task would be two agents editing one
        // worktree, and the scheduler serializing their writes makes that
        // survivable rather than intended. This is a guard and not a
        // uniqueness guarantee — two calls racing this read can still both pass
        // — because the durable half of that would be a partial unique index on
        // a column runs move through, and the case it would buy is narrower
        // than the one it would constrain.
        for existing in self.inner.store.retries_of(original)? {
            if !self.inner.store.load_run(existing)?.state().is_terminal() {
                return Err(RuntimeError::RunStillActive { run: existing });
            }
        }
        // The flag is cumulative down a chain of attempts, not a fact about the
        // immediate predecessor. Attempt A writes and fails; attempt B is
        // interrupted before it calls anything; a retry of B has no started
        // call of its own to find, and A's partial write is still on disk. Only
        // carrying the predecessor's own answer forward keeps the warning
        // attached to the workspace it is actually about.
        let workspace_may_be_modified =
            record.workspace_may_be_modified() || self.workspace_may_be_modified(original)?;
        self.start_run_inner(
            record.task_id(),
            agent,
            workspace,
            workspace_metadata,
            Some((original, workspace_may_be_modified)),
        )
    }

    /// Whether any call of `run` reached the point of being able to write.
    ///
    /// Two questions, both answered from what was persisted. Did the call ever
    /// enter `running` — `started_at` is set by that transition and by nothing
    /// else, so it is exactly "past `awaiting_approval`" — and does the tool it
    /// named declare a risk that can change the workspace.
    ///
    /// A tool this build no longer registers counts as one that could write.
    /// The honest answer to "what did that call do" is "this build cannot say",
    /// and the flag exists to warn rather than to reassure.
    fn workspace_may_be_modified(&self, run: RunId) -> Result<bool, RuntimeError> {
        for call in self.inner.store.load_run_tool_calls(run)? {
            if call.started_at().is_none() {
                continue;
            }
            let risk = crate::tool::ToolIdentity::parse(call.tool_id(), call.tool_version())
                .ok()
                .and_then(|identity| {
                    self.inner
                        .registry
                        .get(&identity.id, Some(&identity.version))
                        .map(|tool| tool.descriptor().risk())
                });
            match risk {
                Some(risk) if risk < crate::tool::RiskLevel::WorkspaceWrite => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    fn start_run_inner(
        &self,
        task_id: TaskId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
        workspace_metadata: Option<WorkspaceMetadata>,
        retry: Option<(RunId, bool)>,
    ) -> Result<RunId, RuntimeError> {
        // Held for the whole of this function rather than around the check
        // alone. The row is not the only thing that must land before the claim
        // can be given up: the worker has to be registered as active too, or
        // shutdown would return having cancelled nothing while a thread was
        // still about to start. Shutdown therefore waits for a start already in
        // progress, which is the honest reading of "stop accepting work".
        let _claim = self
            .inner
            .claim
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        // A shut-down coordinator has given its claim up, so a run started
        // under it would be recorded as owned by something nothing is holding —
        // and the very next sweep would be right to interrupt it. Refusing says
        // that plainly rather than persisting a run with no future.
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::LeaseUnavailable {
                reason: "this coordinator has shut down and no longer claims runs".to_owned(),
            });
        }
        let task = self.inner.store.load_task(task_id)?;
        let Some(project_id) = task.project_id() else {
            return Err(RuntimeError::WorkspaceIdentityRequired { task: task_id });
        };
        // Both sides of this comparison go through the store's own redactor, so
        // a front end deriving its reference the same way agrees by
        // construction. The one thing that could make them disagree is a
        // declared secret arriving *between* the two derivations and matching
        // inside the path — the declared-secret set only grows, so an earlier
        // reference can be less redacted than a later one. It stays a comparison
        // rather than a canonicalized check because what is being verified is
        // that the caller named the task's workspace, and the execution root
        // below is taken from the task itself either way.
        let expected = WorkspaceRef::from_task(&task, &**self.inner.store.redactor());
        if workspace.project_id() != expected.project_id() || workspace.root() != expected.root() {
            return Err(RuntimeError::WorkspaceMismatch { task: task_id });
        }
        let workspace_key =
            WorkspaceKey::new(project_id, task.workspace_root()).map_err(|error| {
                RuntimeError::WorkspaceUnavailable {
                    task: task_id,
                    reason: error.to_string(),
                }
            })?;
        if let Some(metadata) = workspace_metadata.as_ref() {
            if metadata.project_id() != workspace_key.project_id() {
                return Err(RuntimeError::WorkspaceMismatch { task: task_id });
            }
            let canonical = std::fs::canonicalize(metadata.canonical_root()).map_err(|error| {
                RuntimeError::WorkspaceUnavailable {
                    task: task_id,
                    reason: format!("catalog workspace metadata is unavailable: {error}"),
                }
            })?;
            if canonical != workspace_key.canonical_root() {
                return Err(RuntimeError::WorkspaceMismatch { task: task_id });
            }
        }
        let policy = self
            .inner
            .policy
            .for_workspace(workspace_key.canonical_root());

        let now = OffsetDateTime::now_utc();
        let run = match retry {
            Some((original, workspace_may_be_modified)) => {
                Run::retrying(task_id, original, workspace_may_be_modified, now)
            }
            None => Run::new(task_id, now),
        };
        let run_id = run.id();
        let queued = RunEvent::new(EventKind::RunStateChanged, now)
            .with_payload(json!({"state": ExecutionState::Queued.as_str()}));
        let owner = Some(self.inner.lease.record());
        match retry {
            // Appended to the *original*, whose own state is untouched: a
            // terminal run's timeline is evidence, and the only honest way to
            // say "this was re-attempted" is to add a line to it. Both writes
            // commit together, so a retry can never exist without the attempt
            // it follows saying so.
            Some((original, workspace_may_be_modified)) => {
                self.inner.store.insert_retry_with_events(
                    &run,
                    owner,
                    queued,
                    original,
                    RunEvent::new(EventKind::RunRetried, now).with_payload(json!({
                        "retry_run_id": run_id.to_string(),
                        "workspace_may_be_modified": workspace_may_be_modified,
                    })),
                )?;
            }
            None => {
                self.inner
                    .store
                    .insert_run_with_event(&run, owner, queued)?;
            }
        }
        self.delivery(run_id);
        if let Err(error) = self.publish(run_id) {
            let _ = self.inner.store.append_event(
                run_id,
                RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc()).with_payload(
                    json!({
                        "kind": "startup_delivery_failed",
                        "error_kind": error.kind(),
                        "message": error.to_string(),
                    }),
                ),
            );
        }

        let cancellation = Cancellation::default();
        self.inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                run_id,
                ActiveRun {
                    cancellation: cancellation.clone(),
                },
            );

        let inner = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name(format!("harkness-run-{run_id}"))
            .spawn(move || {
                RunWorker::new(
                    inner,
                    run_id,
                    task,
                    workspace,
                    workspace_key,
                    workspace_metadata,
                    policy,
                    cancellation,
                    agent,
                )
                .drive();
            });
        if let Err(error) = spawned {
            self.inner
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&run_id);
            let failure = Failure::new("worker_spawn_failed", error.to_string());
            let at = OffsetDateTime::now_utc();
            let _ = self.inner.store.fail_run_with_event(
                run_id,
                failure,
                at,
                run_state_event(ExecutionState::Failed, at),
            );
            let _ = self.publish(run_id);
            self.close_delivery(run_id);
            return Err(RuntimeError::WorkerSpawn {
                run: run_id,
                reason: error.to_string(),
            });
        }
        Ok(run_id)
    }

    /// Cancels queued work, in-flight execution, or a parked approval wait.
    pub fn cancel_run(&self, run: RunId) -> Result<(), RuntimeError> {
        let cancellation = self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&run)
            .map(|active| active.cancellation.clone())
            .ok_or(RuntimeError::RunNotActive { run })?;
        tracing::info!(run_id = %run, "cancellation requested");
        cancellation.cancel();
        self.inner.scheduler.cancel_run(run);

        for request in self.inner.store.run_approvals(run)? {
            if request.state() != ApprovalState::Pending {
                continue;
            }
            match self.inner.store.resolve_approval(
                request.id(),
                ApprovalState::Cancelled,
                OffsetDateTime::now_utc(),
            ) {
                Ok((resolved, _)) => self.inner.approvals.resolve_from(&resolved),
                Err(_) => {
                    if let Ok(resolved) = self.inner.store.approval(request.id()) {
                        self.inner.approvals.resolve_from(&resolved);
                    }
                }
            }
        }
        self.publish(run)?;
        Ok(())
    }

    /// Persists an approval decision, then wakes its parked run.
    pub fn decide_approval(&self, decision: ApprovalDecision) -> Result<(), RuntimeError> {
        let approval = decision.approval_id();
        let current = self.inner.store.approval(approval)?;
        let run = current.run_id();
        if !self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&run)
        {
            return Err(RuntimeError::ApprovalNotActive { approval });
        }
        let (resolved, _) = self.inner.store.decide_approval(approval, decision)?;
        tracing::info!(
            run_id = %run,
            tool_call_id = %resolved.tool_call_id(),
            approval_id = %approval,
            verdict = resolved.state().as_str(),
            "approval decided"
        );
        self.inner.approvals.resolve_from(&resolved);
        self.publish(run)?;
        Ok(())
    }

    /// Loads a complete, consistent-enough read view from durable records.
    pub fn run_snapshot(&self, run_id: RunId) -> Result<RunSnapshot, RuntimeError> {
        let run = self.inner.store.load_run(run_id)?;
        let task = self.inner.store.load_task(run.task_id())?;
        let events = load_all_events(&self.inner.store, run_id)?;
        Ok(RunSnapshot {
            task,
            run,
            steps: self.inner.store.load_run_steps(run_id)?,
            tool_calls: self.inner.store.load_run_tool_calls(run_id)?,
            approvals: self.inner.store.run_approvals(run_id)?,
            artifacts: self.inner.store.run_artifacts(run_id)?,
            events,
        })
    }

    /// Subscribes to persisted events, replaying existing history first.
    pub fn subscribe(&self, run: RunId) -> Result<EventReceiver, RuntimeError> {
        let record = self.inner.store.load_run(run)?;
        let delivery = self.delivery(run);
        let mut state = delivery
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let replay_tip = self.inner.store.latest_event_seq(run)?;
        let subscriber = Arc::new(subscription::Subscriber::new(replay_tip));
        // `closed` is set by the worker that drove the run, so it is only ever
        // true for a run *this* coordinator started. A run that reached a
        // terminal state in another process — after a restart, from a second
        // front end, or simply a historical run — mints a fresh open delivery
        // here, and nothing will ever publish to it or close it: the receiver
        // replays the durable history and then blocks forever, with `try_recv`
        // reporting `Empty` rather than `Disconnected`. The run's own recorded
        // state is the authority when no worker is left to speak for it.
        if state.closed || (record.state().is_terminal() && !self.is_active(run)) {
            subscriber.close();
        } else {
            state.subscribers.push(Arc::downgrade(&subscriber));
        }
        Ok(EventReceiver::new(
            subscriber,
            Arc::clone(&self.inner.store),
            run,
            replay_tip,
        ))
    }

    /// Delegates newest-first run listing to the durable store.
    pub fn list_runs(&self, page: RunPage) -> Result<crate::store::RunListing, RuntimeError> {
        Ok(self.inner.store.list_runs(page)?)
    }

    /// Reads one page of a run's timeline, in either direction.
    ///
    /// The paged counterpart of [`run_snapshot`](Self::run_snapshot), whose
    /// `events` field is the whole log: a surface rendering a long run wants the
    /// newest entries and then older ones on demand, and materializing every
    /// event to show twenty of them is the cost this avoids.
    ///
    /// Unlike [`Store::event_page`], an unknown run is
    /// [`RuntimeError`], not an empty page. A timeline is always asked for by a
    /// caller that believes the run exists, and answering "no events" would let
    /// a mistyped identifier read as an empty run.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when `run_id` names no stored run, and when the
    /// page limit is outside the store's bounds.
    pub fn event_page(
        &self,
        run_id: RunId,
        page: EventPage,
    ) -> Result<crate::store::EventListing, RuntimeError> {
        self.inner.store.load_run(run_id)?;
        Ok(self.inner.store.event_page(run_id, page)?)
    }

    /// Lists every unanswered approval request across every run, oldest first.
    ///
    /// This is what a front end reads on start-up and after any decision. The
    /// listing is unpaged because the pending set is bounded by construction:
    /// a request exists only while a call is parked waiting for it, and the
    /// scheduler caps how many calls can be in flight at once. Answered
    /// requests leave it immediately and are read back through
    /// [`run_snapshot`](Self::run_snapshot) instead.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when the listing statement fails.
    pub fn pending_approvals(&self) -> Result<Vec<ApprovalRequest>, RuntimeError> {
        Ok(self.inner.store.pending_approvals()?)
    }

    /// Whether a worker in this process is still driving `run`.
    ///
    /// A terminal record plus no live worker means nothing will ever publish
    /// again, which is what lets a subscription close instead of waiting.
    fn is_active(&self, run: RunId) -> bool {
        self.inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&run)
    }

    fn delivery(&self, run: RunId) -> Arc<RunDelivery> {
        Arc::clone(
            self.inner
                .deliveries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(run)
                .or_default(),
        )
    }

    fn publish(&self, run: RunId) -> Result<(), RuntimeError> {
        publish(&self.inner, run).map_err(Into::into)
    }

    fn close_delivery(&self, run: RunId) {
        close_delivery(&self.inner, run);
    }
}

impl CoordinatorInner {
    /// Cooperatively stops everything this coordinator owns, then lets the
    /// claim go.
    ///
    /// The order is the whole point. Cancelling first gives a live run the
    /// chance to record its own ending, which is always better evidence than an
    /// inferred one. Releasing the lease afterwards is what makes the runs that
    /// did *not* finish findable: the next start reads the released claim and
    /// ends them, rather than leaving them looking live for good.
    fn shutdown(&self) {
        {
            // Exclusive, so a start that already passed its own check has
            // finished recording and registering its run before the claim is
            // given up, and one that has not yet begun sees the flag.
            let _claim = self.claim.write().unwrap_or_else(PoisonError::into_inner);
            if self.stopping.swap(true, Ordering::AcqRel) {
                return;
            }
        }
        // Taking the housekeeping mutex before signalling is what makes the
        // wake-up reliable: the renewal thread re-reads `stopping` while
        // holding it, so it either observes the flag or is already parked on
        // the condition variable when this notification arrives. Signalling
        // without it loses the wake-up whenever the thread is between those two
        // points, and the coordinator then keeps a store open for a further
        // renewal interval after its last handle is gone.
        drop(
            self.housekeeping_state
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        self.housekeeping.notify_all();

        let active = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(run, state)| (*run, state.cancellation.clone()))
            .collect::<Vec<_>>();
        for (run, cancellation) in &active {
            cancellation.cancel();
            self.scheduler.cancel_run(*run);
        }

        let deadline = std::time::Instant::now() + SHUTDOWN_DEADLINE;
        while std::time::Instant::now() < deadline {
            if self
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
            {
                break;
            }
            thread::sleep(WAIT_SLICE);
        }

        // Recorded before the lock is dropped, so a reader that sees the row
        // released is never told "live" by a file this process is about to let
        // go of. The reverse order would leave a window answering the opposite.
        let _ = self
            .store
            .release_lease(self.lease.id(), OffsetDateTime::now_utc());
        self.lease.release();
    }
}

impl Drop for CoordinatorInner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn publish(inner: &CoordinatorInner, run: RunId) -> Result<(), crate::store::StoreError> {
    let delivery = Arc::clone(
        inner
            .deliveries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(run)
            .or_default(),
    );
    let mut state = delivery
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    loop {
        let page = inner
            .store
            .events(run, state.cursor, DEFAULT_EVENT_PAGE_LIMIT)?;
        if page.is_empty() {
            break;
        }
        for event in page {
            state.cursor = Some(event.seq);
            state.subscribers.retain(|subscriber| {
                subscriber.upgrade().is_some_and(|subscriber| {
                    subscriber.push(event.clone());
                    true
                })
            });
        }
    }
    Ok(())
}

fn close_delivery(inner: &CoordinatorInner, run: RunId) {
    let delivery = inner
        .deliveries
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&run)
        .cloned();
    let Some(delivery) = delivery else {
        return;
    };
    let mut state = delivery
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    state.closed = true;
    state.subscribers.retain(|subscriber| {
        subscriber.upgrade().is_some_and(|subscriber| {
            subscriber.close();
            true
        })
    });
}

fn load_all_events(
    store: &Store,
    run: RunId,
) -> Result<Vec<StoredEvent>, crate::store::StoreError> {
    let mut events = Vec::new();
    let mut after = None;
    loop {
        let page = store.events(run, after, DEFAULT_EVENT_PAGE_LIMIT)?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|event| event.seq);
        events.extend(page);
    }
    Ok(events)
}

fn run_state_event(state: ExecutionState, at: OffsetDateTime) -> RunEvent {
    RunEvent::new(EventKind::RunStateChanged, at).with_payload(json!({"state": state.as_str()}))
}

struct RunWorker {
    inner: Arc<CoordinatorInner>,
    run: RunId,
    task: Task,
    workspace: WorkspaceRef,
    workspace_key: WorkspaceKey,
    workspace_metadata: Option<WorkspaceMetadata>,
    policy: PolicyEngine,
    cancellation: Cancellation,
    agent: Box<dyn Agent>,
    planned: VecDeque<StepId>,
    next_ordinal: u32,
}

#[derive(Debug)]
struct WorkerFault {
    kind: &'static str,
    message: String,
}

impl WorkerFault {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: truncate_failure_text(message.into(), MAX_FAILURE_MESSAGE_BYTES),
        }
    }
}

impl RunWorker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        inner: Arc<CoordinatorInner>,
        run: RunId,
        task: Task,
        workspace: WorkspaceRef,
        workspace_key: WorkspaceKey,
        workspace_metadata: Option<WorkspaceMetadata>,
        policy: PolicyEngine,
        cancellation: Cancellation,
        agent: Box<dyn Agent>,
    ) -> Self {
        Self {
            inner,
            run,
            task,
            workspace,
            workspace_key,
            workspace_metadata,
            policy,
            cancellation,
            agent,
            planned: VecDeque::new(),
            next_ordinal: 0,
        }
    }

    fn drive(mut self) {
        // The worker's whole life happens inside one span, on the thread that
        // owns the run. Everything it opens beneath this — a step, a tool call,
        // an approval wait — names `run_id` itself as well, because the executor
        // and the scheduler do their work on other threads and would otherwise
        // lose the only field that makes the log searchable.
        let span = observe::run_span(self.run);
        let _entered = span.enter();
        self.inner
            .store
            .set_run_owner(self.run, Some(std::process::id()))
            .ok();
        let outcome = catch_unwind(AssertUnwindSafe(|| self.drive_inner()));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(fault)) => self.record_fault(fault),
            Err(payload) => self.record_fault(WorkerFault::new(
                "coordinator_panicked",
                panic_payload(&*payload),
            )),
        }
        let _ = self.inner.store.set_run_owner(self.run, None);
        let _ = publish(&self.inner, self.run);
        self.inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run);
        close_delivery(&self.inner, self.run);
    }

    fn drive_inner(&mut self) -> Result<(), WorkerFault> {
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .transition_run_with_event(
                self.run,
                ExecutionState::Running,
                at,
                run_state_event(ExecutionState::Running, at),
            )
            .map_err(store_fault)?;
        self.publish()?;
        tracing::info!(
            run_id = %self.run,
            task_id = %self.task.id(),
            state = "running",
            "run started"
        );

        let mut observation = Observation::RunStarted {
            task: TaskRef::from_task(&self.task, &**self.inner.store.redactor()),
            workspace: self.workspace.clone(),
        };
        for _ in 0..MAX_AGENT_TURNS {
            if self.cancellation.is_cancelled() {
                return self.finish_cancelled(None);
            }

            self.record_observation(&observation)?;
            let action = catch_unwind(AssertUnwindSafe(|| {
                self.agent.next_action(observation.clone())
            }))
            .map_err(|payload| WorkerFault::new("agent_panicked", panic_payload(&*payload)))?;
            if self.cancellation.is_cancelled() {
                return self.finish_cancelled(None);
            }
            self.record_action(&action)?;
            self.record_checkpoint()?;
            if self.cancellation.is_cancelled() {
                return self.finish_cancelled(None);
            }

            match action {
                AgentAction::Plan { steps } => {
                    self.persist_plan(steps)?;
                    // The agent seam has no separate plan-acknowledged
                    // observation. Re-offering the last observation lets the
                    // next scripted turn proceed without inventing a wire
                    // variant that would require a schema migration.
                }
                AgentAction::CallTool {
                    tool_id,
                    tool_version,
                    input,
                } => {
                    let Some(next) = self.dispatch_tool(tool_id, tool_version, input)? else {
                        return Ok(());
                    };
                    observation = next;
                }
                AgentAction::CompleteRun { summary } => {
                    self.terminalize_unexecuted_steps()?;
                    let at = OffsetDateTime::now_utc();
                    self.inner
                        .store
                        .transition_run_with_event(
                            self.run,
                            ExecutionState::Succeeded,
                            at,
                            run_state_event(ExecutionState::Succeeded, at)
                                .with_payload(json!({"state": "succeeded", "summary": summary})),
                        )
                        .map_err(store_fault)?;
                    self.publish()?;
                    tracing::info!(run_id = %self.run, state = "succeeded", "run finished");
                    return Ok(());
                }
                AgentAction::FailRun { reason } => {
                    self.terminalize_unexecuted_steps()?;
                    let at = OffsetDateTime::now_utc();
                    let message = serde_json::to_string(&reason)
                        .unwrap_or_else(|_| "the agent failed the run".to_owned());
                    self.inner
                        .store
                        .fail_run_with_event(
                            self.run,
                            Failure::new(reason.kind(), message),
                            at,
                            run_state_event(ExecutionState::Failed, at),
                        )
                        .map_err(store_fault)?;
                    self.publish()?;
                    tracing::warn!(
                        run_id = %self.run,
                        state = "failed",
                        kind = reason.kind(),
                        "the agent failed the run"
                    );
                    return Ok(());
                }
            }
        }
        Err(WorkerFault::new(
            "agent_turn_limit",
            format!("the agent exceeded the {MAX_AGENT_TURNS}-turn run limit"),
        ))
    }

    fn record_observation(&self, observation: &Observation) -> Result<(), WorkerFault> {
        let payload = ObservationRecord::new(observation.clone())
            .to_event_payload()
            .map_err(|error| WorkerFault::new("agent_record_invalid", error.to_string()))?;
        self.inner
            .store
            .append_event(
                self.run,
                RunEvent::new(EventKind::AgentObservation, OffsetDateTime::now_utc())
                    .with_payload(payload),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn record_action(&self, action: &AgentAction) -> Result<(), WorkerFault> {
        let payload = AgentActionRecord::new(action.clone(), &**self.inner.store.redactor())
            .to_event_payload()
            .map_err(|error| WorkerFault::new("agent_record_invalid", error.to_string()))?;
        self.inner
            .store
            .append_event(
                self.run,
                RunEvent::new(EventKind::AgentAction, OffsetDateTime::now_utc())
                    .with_payload(payload),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn record_checkpoint(&self) -> Result<(), WorkerFault> {
        let state = catch_unwind(AssertUnwindSafe(|| self.agent.state()))
            .map_err(|payload| WorkerFault::new("agent_panicked", panic_payload(&*payload)))?;
        self.inner
            .store
            .append_event(
                self.run,
                RunEvent::new(EventKind::AgentCheckpoint, OffsetDateTime::now_utc())
                    .with_payload(state.to_event_payload()),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn persist_plan(&mut self, steps: Vec<crate::agent::PlannedStep>) -> Result<(), WorkerFault> {
        for planned in steps {
            let at = OffsetDateTime::now_utc();
            let step = Step::new(self.run, self.next_ordinal, planned.title, at);
            self.next_ordinal = self
                .next_ordinal
                .checked_add(1)
                .ok_or_else(|| WorkerFault::new("step_limit", "step ordinal overflow"))?;
            self.inner.store.insert_step(&step).map_err(store_fault)?;
            self.inner
                .store
                .append_event(
                    self.run,
                    RunEvent::new(EventKind::Diagnostic, at)
                        .for_step(step.id())
                        .with_payload(json!({"kind": "step_planned", "title": step.title()})),
                )
                .map_err(store_fault)?;
            self.planned.push_back(step.id());
        }
        self.publish()
    }

    fn begin_step(&mut self, title: &str) -> Result<Step, WorkerFault> {
        let mut step = if let Some(id) = self.planned.pop_front() {
            self.inner.store.load_step(id).map_err(store_fault)?
        } else {
            let step = Step::new(
                self.run,
                self.next_ordinal,
                title,
                OffsetDateTime::now_utc(),
            );
            self.next_ordinal = self
                .next_ordinal
                .checked_add(1)
                .ok_or_else(|| WorkerFault::new("step_limit", "step ordinal overflow"))?;
            self.inner.store.insert_step(&step).map_err(store_fault)?;
            step
        };
        let at = OffsetDateTime::now_utc();
        (step, _) = self
            .inner
            .store
            .transition_step_with_event(
                step.id(),
                ExecutionState::Running,
                at,
                RunEvent::new(EventKind::StepStarted, at)
                    .for_step(step.id())
                    .with_payload(json!({"state": "running"})),
            )
            .map_err(store_fault)?;
        self.publish()?;
        Ok(step)
    }

    fn dispatch_tool(
        &mut self,
        tool_id: crate::tool::ToolId,
        tool_version: crate::tool::ToolVersion,
        input: Value,
    ) -> Result<Option<Observation>, WorkerFault> {
        let step = self.begin_step(&format!("Run {tool_id}"))?;
        let at = OffsetDateTime::now_utc();
        let call = ToolCall::new(
            &step,
            tool_id.to_string(),
            tool_version.to_string(),
            input.clone(),
            at,
        );
        let call_id = call.id();
        // Opened around the whole dispatch, so policy, approval and admission
        // events all land under the step and call they belong to. The fields are
        // repeated rather than inherited for the reason `observe` documents:
        // the executor supervises on another thread entirely.
        let step_span = observe::step_span(self.run, step.id());
        let _entered = step_span.enter();
        tracing::debug!(
            run_id = %self.run,
            step_id = %step.id(),
            tool_call_id = %call_id,
            tool_id = %tool_id,
            tool_version = %tool_version,
            "tool requested"
        );
        self.inner
            .store
            .insert_tool_call(&call)
            .map_err(store_fault)?;

        let Some(tool) = self
            .inner
            .registry
            .get(&tool_id, Some(&tool_version))
            .cloned()
        else {
            let error = self
                .inner
                .registry
                .resolve(&tool_id, Some(&tool_version))
                .expect_err("get and resolve agree on a missing tool");
            return self.fail_before_policy(&step, call_id, error.kind(), error.to_string());
        };
        let raw = serde_json::value::to_raw_value(&input)
            .map_err(|error| WorkerFault::new("invalid_input", error.to_string()))?;
        let boundary = PathBoundary::new(
            self.workspace_key.canonical_root(),
            std::iter::empty::<&std::path::Path>(),
        )
        .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let prepared = match tool.prepare_json(&raw, &boundary) {
            Ok(prepared) => prepared,
            Err(error) => {
                return self.fail_before_policy(&step, call_id, error.kind(), error.to_string());
            }
        };
        if self.cancellation.is_cancelled() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_tool_call_with_event(
                    call_id,
                    ToolCallState::Cancelled,
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call_id)
                        .with_payload(json!({"state": "cancelled"})),
                )
                .map_err(store_fault)?;
            self.finish_cancelled(Some(step.id()))?;
            return Ok(None);
        }

        let input_hash = canonical_input_hash(&input)
            .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let binding = WorkspaceBinding::new(
            Some(self.workspace_key.project_id()),
            self.workspace_key.canonical_root(),
        );
        let identity = tool.descriptor().identity().clone();
        let grants = self.inner.store.run_grants(self.run).map_err(store_fault)?;
        let candidate = CandidateCall::new(self.run, call_id, &binding, &identity, input_hash)
            .with_capabilities(tool.descriptor().capabilities());
        let matching = matching_grants(&grants, &candidate);
        let trust = self
            .inner
            .store
            .resolve_workspace_trust(
                self.workspace_key.project_id(),
                self.workspace_key.canonical_root(),
            )
            .map_err(store_fault)?;
        let request = PolicyRequest::new(
            tool.descriptor(),
            prepared.classification(),
            trust,
            ExecutionMode::Interactive,
        )
        .with_paths(prepared.paths())
        .with_grants(&matching);
        let decision = self.policy.evaluate(&request);
        let risk = request.risk();
        if self.cancellation.is_cancelled() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_tool_call_with_event(
                    call_id,
                    ToolCallState::Cancelled,
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call_id)
                        .with_payload(json!({"state": "cancelled"})),
                )
                .map_err(store_fault)?;
            self.finish_cancelled(Some(step.id()))?;
            return Ok(None);
        }
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .apply_tool_call_policy_decision_with_event(
                call_id,
                decision.clone(),
                at,
                RunEvent::new(EventKind::PolicyDecision, at)
                    .for_step(step.id())
                    .for_tool_call(call_id)
                    .with_payload(json!({
                        "decision": decision,
                        "risk": risk.as_str(),
                    })),
            )
            .map_err(store_fault)?;
        self.publish()?;
        // The one event that says why a call did or did not run. `decision` is
        // a field rather than prose so a log can be filtered to every `ask` in a
        // run without matching on a sentence somebody may reword.
        tracing::info!(
            run_id = %self.run,
            step_id = %step.id(),
            tool_call_id = %call_id,
            tool_id = %identity.id,
            tool_version = %identity.version,
            decision = decision.verdict().as_str(),
            risk = risk.as_str(),
            "policy decided"
        );

        match decision.verdict() {
            PolicyVerdict::Allow => self.execute_scheduled(step, call_id, risk, None),
            PolicyVerdict::Deny => {
                self.finish_step_failed(step.id(), Failure::new("policy", decision.reason()))?;
                Ok(Some(Observation::PolicyDenied {
                    call: call_id,
                    reason: crate::agent::RedactedText::new(
                        decision.reason(),
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
            PolicyVerdict::Ask => {
                self.await_approval(step, call_id, input_hash, binding, identity, risk)
            }
        }
    }

    fn fail_before_policy(
        &self,
        step: &Step,
        call: crate::domain::ToolCallId,
        kind: &str,
        message: String,
    ) -> Result<Option<Observation>, WorkerFault> {
        // Clamped, for the reason `ToolError::as_failure` clamps: the store
        // refuses an oversized `failure_message`, and here that refusal becomes
        // a `WorkerFault` that fails the whole *run* rather than handing the
        // agent a recoverable `ToolFailed`. A caller-chosen path is enough to
        // reach it — a boundary refusal embeds the path it was given, so an
        // input that fit the inline bound produces a message that does not.
        let message = truncate_failure_text(message, MAX_FAILURE_MESSAGE_BYTES);
        let failure = Failure::new(kind, &message);
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .fail_tool_call_with_event(
                call,
                failure.clone(),
                at,
                RunEvent::new(EventKind::ToolCallStateChanged, at)
                    .for_step(step.id())
                    .for_tool_call(call)
                    .with_payload(json!({"state": "failed", "kind": kind})),
            )
            .map_err(store_fault)?;
        self.finish_step_failed(step.id(), failure)?;
        self.publish()?;
        Ok(Some(Observation::ToolFailed {
            call,
            error: ToolErrorView::new(kind, message, &**self.inner.store.redactor()),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn await_approval(
        &self,
        step: Step,
        call: crate::domain::ToolCallId,
        input_hash: crate::approval::InputHash,
        binding: WorkspaceBinding,
        identity: crate::tool::ToolIdentity,
        risk: crate::tool::RiskLevel,
    ) -> Result<Option<Observation>, WorkerFault> {
        let one_call_only = self
            .inner
            .store
            .load_tool_call(call)
            .map_err(store_fault)?
            .policy_decision()
            .is_some_and(|decision| decision.one_call_only());
        let requested_scope = if one_call_only {
            ApprovalScope::ExactCall
        } else {
            ApprovalScope::ToolForRun
        };
        let pending = PendingApproval::new(
            self.run,
            call,
            identity.clone(),
            input_hash,
            binding.clone(),
            risk,
            OffsetDateTime::now_utc(),
        )
        .requesting(requested_scope)
        .with_capabilities(
            self.inner
                .registry
                .get(&identity.id, Some(&identity.version))
                .expect("the prepared tool remains registered")
                .descriptor()
                .capabilities()
                .iter()
                .cloned(),
        )
        .summarized_as(format!("request to run {identity}"));
        let request = ApprovalRequest::open(pending)
            .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let approval_id = request.id();
        let ticket = self.inner.approvals.ticket(request.id()).ok_or_else(|| {
            WorkerFault::new("approval_waiter_exists", "approval already has a waiter")
        })?;
        self.inner
            .store
            .open_approval(request)
            .map_err(store_fault)?;
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .transition_run_with_event(
                self.run,
                ExecutionState::WaitingForApproval,
                at,
                run_state_event(ExecutionState::WaitingForApproval, at),
            )
            .map_err(store_fault)?;
        self.publish()?;

        // A span rather than a pair of events, because how long a run waited on
        // a person is one of the few durations worth reading straight off a log,
        // and two events make a reader compute it.
        let waiting = observe::approval_span(self.run, call, approval_id);
        let _entered = waiting.enter();
        tracing::info!(
            run_id = %self.run,
            tool_call_id = %call,
            approval_id = %approval_id,
            requested_scope = requested_scope.as_str(),
            risk = risk.as_str(),
            "waiting for an approval decision"
        );
        self.wait_approval_ticket(ticket, step, call, input_hash, binding, identity, risk)
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_approval_ticket(
        &self,
        mut ticket: crate::approval::ApprovalTicket<'_>,
        step: Step,
        call: crate::domain::ToolCallId,
        input_hash: crate::approval::InputHash,
        binding: WorkspaceBinding,
        identity: crate::tool::ToolIdentity,
        risk: crate::tool::RiskLevel,
    ) -> Result<Option<Observation>, WorkerFault> {
        loop {
            match ticket.wait_for(WAIT_SLICE) {
                Ok(observation) => {
                    return self.resume_approval(
                        observation,
                        step,
                        call,
                        input_hash,
                        binding,
                        identity,
                        risk,
                    );
                }
                Err(returned) => {
                    ticket = returned;
                    self.publish()?;
                    if self.cancellation.is_cancelled() {
                        if let Ok((resolved, _)) = self.inner.store.resolve_approval(
                            ticket.approval_id(),
                            ApprovalState::Cancelled,
                            OffsetDateTime::now_utc(),
                        ) {
                            self.inner.approvals.resolve_from(&resolved);
                        } else if let Ok(resolved) = self.inner.store.approval(ticket.approval_id())
                        {
                            self.inner.approvals.resolve_from(&resolved);
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resume_approval(
        &self,
        observation: crate::approval::ApprovalObservation,
        step: Step,
        call: crate::domain::ToolCallId,
        input_hash: crate::approval::InputHash,
        binding: WorkspaceBinding,
        identity: crate::tool::ToolIdentity,
        risk: crate::tool::RiskLevel,
    ) -> Result<Option<Observation>, WorkerFault> {
        let request = self
            .inner
            .store
            .approval(observation.approval_id())
            .map_err(store_fault)?;
        if observation.state() == ApprovalState::Cancelled || self.cancellation.is_cancelled() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_tool_call_with_event(
                    call,
                    ToolCallState::Cancelled,
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call)
                        .with_payload(json!({"state": "cancelled"})),
                )
                .map_err(store_fault)?;
            self.finish_cancelled(Some(step.id()))?;
            return Ok(None);
        }

        let decided_by = request
            .decision()
            .map(|decision| decision.decided_via().as_str())
            .unwrap_or("approval_service");
        if observation.verdict() == ApprovalVerdict::Denied {
            let failure = Failure::new(
                "approval_denied",
                observation.reason().unwrap_or("the approval was denied"),
            );
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .reject_tool_call_approval_with_event(
                    call,
                    decided_by,
                    failure.clone(),
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call)
                        .with_payload(json!({"state": "denied", "kind": "approval_denied"})),
                )
                .map_err(store_fault)?;
            self.inner
                .store
                .resume_run_after_denial_with_event(
                    self.run,
                    decided_by,
                    at,
                    run_state_event(ExecutionState::Running, at),
                )
                .map_err(store_fault)?;
            self.finish_step_failed(step.id(), failure)?;
            self.publish()?;
            return Ok(Some(Observation::ApprovalOutcome {
                call,
                outcome: crate::agent::ApprovalOutcomeView::Denied,
            }));
        }

        let grant = request.grant().ok_or_else(|| {
            WorkerFault::new(
                "approval_binding_mismatch",
                "a granted request produced no grant",
            )
        })?;
        let tool = self
            .inner
            .registry
            .get(&identity.id, Some(&identity.version))
            .ok_or_else(|| {
                WorkerFault::new("approval_binding_mismatch", "approved tool disappeared")
            })?;
        let candidate = CandidateCall::new(self.run, call, &binding, &identity, input_hash)
            .with_capabilities(tool.descriptor().capabilities());
        if !grant_applies(&grant, &candidate) {
            return Err(WorkerFault::new(
                "approval_binding_mismatch",
                "the persisted approval does not bind to the exact scheduled request",
            ));
        }
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .approve_run_with_event(
                self.run,
                decided_by,
                at,
                run_state_event(ExecutionState::Running, at),
            )
            .map_err(store_fault)?;
        self.publish()?;
        self.execute_scheduled(
            step,
            call,
            risk,
            Some((decided_by.to_owned(), identity, input_hash)),
        )
    }

    fn execute_scheduled(
        &self,
        step: Step,
        call: crate::domain::ToolCallId,
        risk: crate::tool::RiskLevel,
        approval: Option<(
            String,
            crate::tool::ToolIdentity,
            crate::approval::InputHash,
        )>,
    ) -> Result<Option<Observation>, WorkerFault> {
        let mut scheduled = ScheduledCall::new(
            call,
            self.workspace_key.clone(),
            risk,
            self.cancellation.clone(),
        );
        if let Some(metadata) = self.workspace_metadata.as_ref() {
            scheduled = scheduled
                .with_workspace_metadata(metadata.clone())
                .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        }
        if let Some((decided_by, expected_tool, expected_input_hash)) = approval {
            scheduled =
                scheduled.approved_with_binding(decided_by, expected_tool, expected_input_hash);
        }
        let mut ticket = self
            .inner
            .scheduler
            .submit(scheduled)
            .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let completed = loop {
            match ticket.wait_for(WAIT_SLICE) {
                Ok(outcome) => break outcome,
                Err(returned) => {
                    ticket = returned;
                    self.publish()?;
                }
            }
        }
        .map_err(|error| match error {
            crate::schedule::ScheduleError::Execution { source, .. } => match *source {
                ExecutionError::Store(error) => store_fault(error),
                source => WorkerFault::new(source.kind(), source.to_string()),
            },
            error => WorkerFault::new(error.kind(), error.to_string()),
        })?;
        self.publish()?;

        match completed.outcome() {
            CallOutcome::Succeeded { output } => {
                self.finish_step(step.id(), ExecutionState::Succeeded)?;
                let artifacts = self
                    .inner
                    .store
                    .run_artifacts(self.run)
                    .map_err(store_fault)?
                    .into_iter()
                    .filter(|artifact| artifact.tool_call_id() == Some(call))
                    .map(|artifact| artifact.reference())
                    .collect();
                Ok(Some(Observation::ToolResult {
                    call,
                    result: ToolResultView::with_artifacts(
                        output.clone(),
                        artifacts,
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
            CallOutcome::Failed { .. } | CallOutcome::TimedOut { .. } => {
                let failure = completed
                    .record()
                    .failure()
                    .cloned()
                    .unwrap_or_else(|| Failure::new("execution_failed", "the tool failed"));
                self.finish_step_failed(step.id(), failure.clone())?;
                Ok(Some(Observation::ToolFailed {
                    call,
                    error: ToolErrorView::new(
                        failure.kind(),
                        failure.message(),
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
            CallOutcome::Cancelled => {
                self.finish_cancelled(Some(step.id()))?;
                Ok(None)
            }
            // One call ended without a verdict — its worker thread died, which
            // the executor cannot arrange and cannot explain. That is a fact
            // about the *call*, and this run's process is demonstrably alive:
            // it is the one reading this. Marking the run `interrupted` here
            // would make the record claim the owning process stopped, which is
            // the exact lie recovery exists to remove — `interrupted` is
            // written for a run by the startup sweep and by nothing else.
            //
            // So the step ends and the agent is told, in the same shape every
            // other tool failure reaches it. What to do about a call nobody can
            // account for is a decision, and decisions belong to the agent.
            CallOutcome::Interrupted => {
                self.finish_step(step.id(), ExecutionState::Interrupted)?;
                Ok(Some(Observation::ToolFailed {
                    call,
                    error: ToolErrorView::new(
                        crate::tool::ToolError::Interrupted.kind(),
                        "the tool call was interrupted before it reported an outcome",
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
        }
    }

    fn finish_step(&self, step: StepId, state: ExecutionState) -> Result<(), WorkerFault> {
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .transition_step_with_event(
                step,
                state,
                at,
                RunEvent::new(EventKind::StepFinished, at)
                    .for_step(step)
                    .with_payload(json!({"state": state.as_str()})),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn finish_step_failed(&self, step: StepId, failure: Failure) -> Result<(), WorkerFault> {
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .fail_step_with_event(
                step,
                failure.clone(),
                at,
                RunEvent::new(EventKind::StepFinished, at)
                    .for_step(step)
                    .with_payload(json!({
                        "state": "failed",
                        "kind": failure.kind(),
                    })),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn finish_cancelled(&self, step: Option<StepId>) -> Result<(), WorkerFault> {
        if let Some(step) = step {
            let record = self.inner.store.load_step(step).map_err(store_fault)?;
            if !record.state().is_terminal() {
                self.finish_step(step, ExecutionState::Cancelled)?;
            }
        }
        // A plan the agent already published outlives the step that was
        // running. Terminalizing only the current one left every later planned
        // step `queued` under a run that had reached `Cancelled`, with no
        // worker left to move it and nothing that sweeps it — a terminal run
        // whose own children contradict it. `CompleteRun` and `FailRun` have
        // always drained this queue; the cancel and interrupt exits did not.
        self.terminalize_planned_steps()?;
        let run = self.inner.store.load_run(self.run).map_err(store_fault)?;
        if !run.state().is_terminal() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_run_with_event(
                    self.run,
                    ExecutionState::Cancelled,
                    at,
                    run_state_event(ExecutionState::Cancelled, at),
                )
                .map_err(store_fault)?;
            self.publish()?;
        }
        Ok(())
    }

    fn terminalize_unexecuted_steps(&mut self) -> Result<(), WorkerFault> {
        self.terminalize_planned_steps()?;
        self.planned.clear();
        Ok(())
    }

    /// Cancels every step the agent planned but no worker ever began.
    ///
    /// Borrows shared rather than exclusively so the cancellation exits can
    /// call it: those run while an approval ticket borrows the gate out of
    /// `self.inner`, and draining the queue there would need a second mutable
    /// borrow. Not draining costs nothing — the worker is finished either way.
    fn terminalize_planned_steps(&self) -> Result<(), WorkerFault> {
        for step in &self.planned {
            let record = self.inner.store.load_step(*step).map_err(store_fault)?;
            if !record.state().is_terminal() {
                self.finish_step(*step, ExecutionState::Cancelled)?;
            }
        }
        Ok(())
    }

    fn record_fault(&self, fault: WorkerFault) {
        // The message is instrumentation content rather than a structured value,
        // so it reaches the log through the same redactor the store applies —
        // see `observe::log`, which wraps the writer rather than trusting call
        // sites like this one.
        tracing::error!(
            run_id = %self.run,
            kind = fault.kind,
            message = %fault.message,
            "the coordinator failed the run"
        );
        let at = OffsetDateTime::now_utc();
        for request in self.inner.store.run_approvals(self.run).unwrap_or_default() {
            if request.state() == ApprovalState::Pending
                && let Ok((resolved, _)) =
                    self.inner
                        .store
                        .resolve_approval(request.id(), ApprovalState::Cancelled, at)
            {
                self.inner.approvals.resolve_from(&resolved);
            }
        }
        for call in self
            .inner
            .store
            .load_run_tool_calls(self.run)
            .unwrap_or_default()
        {
            if !call.state().is_terminal() {
                if call.state() == ToolCallState::AwaitingApproval {
                    let _ = self.inner.store.transition_tool_call_with_event(
                        call.id(),
                        ToolCallState::Cancelled,
                        at,
                        RunEvent::new(EventKind::ToolCallStateChanged, at)
                            .for_step(call.step_id())
                            .for_tool_call(call.id())
                            .with_payload(json!({"state": "cancelled", "kind": fault.kind})),
                    );
                } else {
                    let failure = Failure::new(fault.kind, &fault.message);
                    let _ = self.inner.store.fail_tool_call_with_event(
                        call.id(),
                        failure,
                        at,
                        RunEvent::new(EventKind::ToolCallStateChanged, at)
                            .for_step(call.step_id())
                            .for_tool_call(call.id())
                            .with_payload(json!({"state": "failed", "kind": fault.kind})),
                    );
                }
            }
        }
        for step in self
            .inner
            .store
            .load_run_steps(self.run)
            .unwrap_or_default()
        {
            if !step.state().is_terminal() {
                let failure = Failure::new(fault.kind, &fault.message);
                let _ = self.inner.store.fail_step_with_event(
                    step.id(),
                    failure,
                    at,
                    RunEvent::new(EventKind::StepFinished, at)
                        .for_step(step.id())
                        .with_payload(json!({"state": "failed", "kind": fault.kind})),
                );
            }
        }
        let _ = self.inner.store.append_event(
            self.run,
            RunEvent::new(EventKind::Diagnostic, at).with_payload(json!({
                "kind": "coordinator_error",
                "error_kind": fault.kind,
                "message": fault.message,
            })),
        );
        if let Ok(run) = self.inner.store.load_run(self.run)
            && !run.state().is_terminal()
        {
            let _ = self.inner.store.fail_run_with_event(
                self.run,
                Failure::new(fault.kind, fault.message),
                at,
                run_state_event(ExecutionState::Failed, at),
            );
        }
    }

    fn publish(&self) -> Result<(), WorkerFault> {
        publish(&self.inner, self.run).map_err(store_fault)
    }
}

fn panic_payload(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a component panicked with a non-string payload".to_owned())
}

fn store_fault(error: crate::store::StoreError) -> WorkerFault {
    WorkerFault::new(error.kind(), error.to_string())
}

pub use error::RuntimeError;
pub use lease::{LEASE_EXPIRY_GRACE, LEASE_RENEW_INTERVAL};
pub use recovery::{RecoveryFailure, RecoveryReport};
pub use snapshot::RunSnapshot;
pub use subscription::{
    EventDelivery, EventReceiver, ReceiveTimeoutError, SUBSCRIBER_CAPACITY, TryReceiveError,
};
