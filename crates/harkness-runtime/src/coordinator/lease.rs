//! The liveness half of a run claim: an advisory lock file per coordinator.
//!
//! # Why a file lock and not a timestamp
//!
//! A crashed process writes nothing. Whatever ends it — `SIGKILL`, a panic in
//! the runtime, the machine losing power — it gets no chance to record that its
//! runs are abandoned, so no column can be trusted to say so. What the kernel
//! *does* guarantee is that every advisory lock a process holds is released
//! when it dies. A lock that can be taken is therefore proof that its holder is
//! gone, and it is the only proof available that does not depend on the dead
//! process having cooperated.
//!
//! [`LeaseRecord`](crate::store::LeaseRecord) is the other half: it names the
//! claim, records when it was taken, and is what a run's row points at. The
//! file says whether the claim is live; the row says what the claim was.
//!
//! # What a stale timestamp does and does not mean
//!
//! `renewed_at` is refreshed on [`LEASE_RENEW_INTERVAL`] by the owning
//! coordinator's housekeeping thread, and it may only ever *widen* the window
//! in which a lease is treated as alive. A held lock outranks any timestamp: a
//! process that is alive but wedged still holds the workspace its runs are
//! mutating, and marking those runs `interrupted` would put a false ending in
//! the history of work that is still, in every sense that matters to a
//! filesystem, in flight. Timestamps decide only the case where the lock cannot
//! be probed at all, and there they decide it late, after
//! [`LEASE_EXPIRY_GRACE`].
//!
//! # Lock ordering
//!
//! These locks are not a fifth concurrency mechanism. A lease lock is taken
//! once, at coordinator construction, and held for the coordinator's life; the
//! recovery lock is taken and released inside one sweep. Neither is ever held
//! while the scheduler, the repository lock, or the catalog lock is acquired,
//! and neither is taken while a store transaction is open.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::domain::LeaseId;
use crate::store::{InterruptionReason, LeaseRecord};

use super::RuntimeError;

/// Directory beneath the Harkness data directory holding every lock file.
///
/// The same name `harkness-git` and `harkness-core` use, for the same reason:
/// locks belong beside the data they guard and never inside a user's
/// repository.
const LOCKS_DIRECTORY: &str = "locks";

/// How often a live coordinator refreshes its lease timestamp.
///
/// Configuration with a documented default rather than a magic number: it is
/// the unit [`LEASE_EXPIRY_GRACE`] is stated in.
pub const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(15);

/// How long an unprobeable lease survives past its last renewal.
///
/// Three renewal intervals, so two missed refreshes are never enough. This is
/// reached only when the lock file cannot be probed at all; a lock that
/// answers "held" keeps its lease alive regardless of how old the timestamp is.
pub const LEASE_EXPIRY_GRACE: Duration = Duration::from_secs(45);

/// How long a starting coordinator waits for another one's sweep to finish.
///
/// Bounded rather than indefinite: a wedged sweeper must not stop an unrelated
/// process from starting. Giving up leaves the abandoned runs for the next
/// start, which is strictly better than refusing to start at all.
const RECOVERY_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a contended lock is retried.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What a lock file says about the process that took it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Liveness {
    /// Somebody holds the lock, so the claim is live.
    Held,
    /// The lock was acquirable, or no lock file exists: nobody holds it.
    Released,
    /// The lock could not be probed. Says nothing either way.
    Unknown(String),
}

/// One coordinator's live claim on the runs it drives.
///
/// The lock is held for as long as this value exists and is released by the
/// kernel if the process dies holding it, which is the whole point.
#[derive(Debug)]
pub(super) struct RuntimeLease {
    record: LeaseRecord,
    path: PathBuf,
    /// Closing this handle releases the lock, including on an abrupt exit.
    file: File,
}

impl RuntimeLease {
    /// Takes a fresh claim under a new identity.
    ///
    /// The lock is taken before anything is recorded, so a lease row can never
    /// exist for a claim whose lock is not yet held — which is the window in
    /// which a concurrent sweep would have been right to call it dead.
    pub(super) fn acquire(data_dir: &Path, at: OffsetDateTime) -> Result<Self, RuntimeError> {
        let id = LeaseId::new();
        let path = lease_path(data_dir, id);
        let unavailable = |reason: String| RuntimeError::LeaseUnavailable { reason };
        fs::create_dir_all(locks_directory(data_dir))
            .map_err(|error| unavailable(format!("the lock directory is unusable: {error}")))?;
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                unavailable(format!("the lease file could not be created: {error}"))
            })?;
        // A fresh identity means a path nothing else has ever opened, so this
        // cannot legitimately be contended. Failing loudly beats waiting for a
        // holder that would have to be a collision in a version-4 UUID.
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => {
                unavailable("a new lease file was already locked".to_owned())
            }
            TryLockError::Error(error) => {
                unavailable(format!("the lease file could not be locked: {error}"))
            }
        })?;
        Ok(Self {
            record: LeaseRecord::acquired(id, std::process::id(), at),
            path,
            file,
        })
    }

    /// Identity of this claim.
    pub(super) fn id(&self) -> LeaseId {
        self.record.id()
    }

    /// The record a run row points at, written with the first run it takes.
    pub(super) fn record(&self) -> &LeaseRecord {
        &self.record
    }

    /// Gives the claim up and removes the file that proved it live.
    ///
    /// Best effort in both halves. The lock is released by closing the handle
    /// whether or not this runs, and a lease file left behind is inert: its
    /// identity is never reused, so nothing will ever consult it again.
    pub(super) fn release(&self) {
        let _ = self.file.unlock();
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Asks the filesystem whether anybody still holds `lease`.
///
/// Never creates the file: a lease file that is not there names a claim nobody
/// is making, and creating one in order to lock it would leave a file per
/// probe behind for a process that is already gone.
pub(super) fn probe(data_dir: &Path, lease: LeaseId) -> Liveness {
    let path = lease_path(data_dir, lease);
    let file = match File::options().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Liveness::Released,
        Err(error) => return Liveness::Unknown(error.to_string()),
    };
    match file.try_lock() {
        // Taken, which means nobody held it. Dropping `file` immediately gives
        // it back; this probe is a question, not a claim.
        Ok(()) => Liveness::Released,
        Err(TryLockError::WouldBlock) => Liveness::Held,
        Err(TryLockError::Error(error)) => Liveness::Unknown(error.to_string()),
    }
}

/// Decides whether a run's claim is dead, and says what proved it.
///
/// `None` means the claim is live, or not yet provably dead. The three
/// certainties — no claim at all, a claim its holder gave up, a lock the kernel
/// has released — are answered immediately; only the case where the lock cannot
/// be probed waits out [`LEASE_EXPIRY_GRACE`], and it waits from the last
/// renewal rather than from now.
pub(super) fn interruption_reason(
    lease: Option<&LeaseRecord>,
    liveness: &Liveness,
    now: OffsetDateTime,
) -> Option<InterruptionReason> {
    let Some(lease) = lease else {
        return Some(InterruptionReason::NoLease);
    };
    if lease.is_released() {
        return Some(InterruptionReason::LeaseReleased);
    }
    match liveness {
        Liveness::Held => None,
        Liveness::Released => Some(InterruptionReason::LeaseLockReleased),
        Liveness::Unknown(_) => {
            let grace = time::Duration::try_from(LEASE_EXPIRY_GRACE)
                .expect("the grace period is representable");
            (now - lease.renewed_at() > grace).then_some(InterruptionReason::LeaseExpired)
        }
    }
}

/// Removes the lock file of a claim that has been proved dead.
///
/// Safe precisely because a lease identity is never reused: no later process
/// will ever open this path, so unlinking it cannot make two coordinators lock
/// two different inodes under one name.
pub(super) fn discard(data_dir: &Path, lease: LeaseId) {
    let _ = fs::remove_file(lease_path(data_dir, lease));
}

/// The short-lived exclusive lock one recovery sweep runs under.
///
/// Two processes starting at once would otherwise both walk the same candidate
/// set. The per-run transaction already refuses the second set of markings —
/// `interrupted` has no outgoing edge — so this is not what makes recovery
/// correct; it is what stops the loser doing the work twice and appending its
/// events into the middle of the winner's.
#[derive(Debug)]
pub(super) struct RecoveryLock {
    file: File,
}

impl RecoveryLock {
    /// Waits briefly for the exclusive sweep lock.
    ///
    /// `Ok(None)` means another process is sweeping and did not finish inside
    /// [`RECOVERY_LOCK_TIMEOUT`]. Its sweep covers the same runs this one would
    /// have, so giving up loses nothing but the report.
    pub(super) fn acquire(data_dir: &Path) -> Result<Option<Self>, RuntimeError> {
        let unavailable = |reason: String| RuntimeError::LeaseUnavailable { reason };
        let directory = locks_directory(data_dir);
        fs::create_dir_all(&directory)
            .map_err(|error| unavailable(format!("the lock directory is unusable: {error}")))?;
        let path = directory.join("runtime-recovery.lock");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                unavailable(format!("the recovery lock could not be opened: {error}"))
            })?;

        let deadline = Instant::now() + RECOVERY_LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(Self { file })),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(unavailable(format!(
                        "the recovery lock could not be taken: {error}"
                    )));
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for RecoveryLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn locks_directory(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCKS_DIRECTORY)
}

fn lease_path(data_dir: &Path, lease: LeaseId) -> PathBuf {
    locks_directory(data_dir).join(format!("runtime-lease-{lease}.lock"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;
    use time::OffsetDateTime;

    use crate::domain::LeaseId;
    use crate::store::{InterruptionReason, LeaseRecord};

    use super::{
        LEASE_EXPIRY_GRACE, Liveness, RecoveryLock, RuntimeLease, interruption_reason, probe,
    };

    fn at(offset: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).unwrap()
    }

    fn record(released: bool) -> LeaseRecord {
        LeaseRecord::from_stored(LeaseId::new(), 42, at(0), at(0), released.then(|| at(1)))
    }

    #[test]
    fn a_held_lease_probes_as_held_and_a_dropped_one_as_released() {
        let data_dir = TempDir::new().unwrap();
        let lease = RuntimeLease::acquire(data_dir.path(), at(0)).unwrap();
        let id = lease.id();

        assert_eq!(probe(data_dir.path(), id), Liveness::Held);

        drop(lease);
        assert_eq!(probe(data_dir.path(), id), Liveness::Released);
    }

    #[test]
    fn a_lease_that_was_never_taken_probes_as_released_without_creating_a_file() {
        let data_dir = TempDir::new().unwrap();
        let id = LeaseId::new();

        assert_eq!(probe(data_dir.path(), id), Liveness::Released);
        assert!(
            !Path::new(data_dir.path()).join("locks").exists(),
            "probing must not create the directory it looked in"
        );
    }

    #[test]
    fn a_held_lock_outranks_every_timestamp() {
        let lease = record(false);
        assert_eq!(
            interruption_reason(Some(&lease), &Liveness::Held, at(0)),
            None
        );
        assert_eq!(
            interruption_reason(Some(&lease), &Liveness::Held, at(100_000)),
            None,
            "a wedged process still holds the workspace its runs are mutating"
        );
    }

    #[test]
    fn an_unprobeable_lease_survives_until_the_grace_period_elapses() {
        let lease = record(false);
        let unknown = Liveness::Unknown("permission denied".to_owned());
        let grace = i64::try_from(LEASE_EXPIRY_GRACE.as_secs()).unwrap();

        assert_eq!(interruption_reason(Some(&lease), &unknown, at(grace)), None);
        assert_eq!(
            interruption_reason(Some(&lease), &unknown, at(grace + 1)),
            Some(InterruptionReason::LeaseExpired)
        );
    }

    #[test]
    fn the_three_certain_answers_do_not_wait_for_a_timestamp() {
        assert_eq!(
            interruption_reason(None, &Liveness::Held, at(0)),
            Some(InterruptionReason::NoLease)
        );
        assert_eq!(
            interruption_reason(Some(&record(true)), &Liveness::Held, at(0)),
            Some(InterruptionReason::LeaseReleased),
            "a claim its holder gave up is over however alive the process is"
        );
        assert_eq!(
            interruption_reason(Some(&record(false)), &Liveness::Released, at(0)),
            Some(InterruptionReason::LeaseLockReleased)
        );
    }

    #[test]
    fn the_recovery_lock_is_exclusive_and_bounded() {
        let data_dir = TempDir::new().unwrap();
        let held = RecoveryLock::acquire(data_dir.path()).unwrap().unwrap();

        let started = std::time::Instant::now();
        let contended = RecoveryLock::acquire(data_dir.path()).unwrap();
        let waited = started.elapsed();

        assert!(contended.is_none(), "the sweep lock admitted two holders");
        assert!(
            waited < std::time::Duration::from_secs(30),
            "the wait was not bounded: {waited:?}"
        );
        drop(held);
        assert!(RecoveryLock::acquire(data_dir.path()).unwrap().is_some());
    }
}
