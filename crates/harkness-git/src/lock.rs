//! The per-repository advisory lock.

use std::{
    fs::{self, File, TryLockError},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use git2::{ErrorCode, Repository};
use uuid::Uuid;

use crate::{GitError, runner::Cancellation};

/// Namespace for the version 5 lock identifiers, fixed forever: changing it
/// would rename every lock file, and two Harkness builds that disagreed about
/// the name would stop excluding each other.
const REPOSITORY_LOCK_NAMESPACE: Uuid = Uuid::from_u128(0x7f3a_9c1e_5b2d_4e6a_9c17_2f8b_41d0_a6e3);

/// Stable repository-lock namespace beneath the embedding application's data
/// directory. Moving it would let old and new Harkness builds mutate one
/// repository concurrently.
const LOCKS_DIRECTORY: &str = "locks";

/// How long a caller waits before the repository is reported busy.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the wait re-tries and re-checks for cancellation.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// An exclusive advisory lock over one Git repository, held while it is
/// mutated.
///
/// # Granularity
///
/// The lock is keyed by the Git *common directory*, so every linked worktree of
/// one repository shares a single lock. Worktrees have their own index and
/// HEAD, but they share `objects`, `refs` and `packed-refs`; treating them as
/// independent would let two Harkness operations write the same object store at
/// once.
///
/// # Location
///
/// The lock file lives below `locks/` in the caller-provided application data
/// directory, never inside the user's `.git`, so acquiring a lock does not
/// write repository metadata.
///
/// # Scope
///
/// Taken for every Git mutation and for no read at all. A torn read corrects
/// itself on the next refresh, whereas locking reads would serialize a status
/// refresh behind a long fetch. Unlike the catalog lock it is held across
/// network operations, because it excludes only the one repository being
/// worked on rather than every project at once.
///
/// # Lock ordering
///
/// **The repository lock is always acquired before the catalog lock, and the
/// catalog lock is never held while acquiring a repository lock.** A caller
/// that needs both learns the repository path under a shared catalog read that
/// it releases, takes this lock, and only then takes the exclusive catalog
/// lock and re-verifies what it read.
///
#[derive(Debug)]
pub(crate) struct RepositoryLock {
    /// The kernel releases the lock when this handle closes, including if the
    /// process dies holding it.
    file: File,
    #[cfg(test)]
    path: PathBuf,
}

impl RepositoryLock {
    /// Takes the lock for the repository at `repository`, waiting briefly.
    ///
    /// The wait is bounded, unlike the catalog lock's: a front end can report
    /// that another operation is already running, which is more useful than
    /// hanging behind a fetch that may take minutes. `cancellation` is polled
    /// throughout, so a caller that gives up first is not made to wait out the
    /// timeout.
    pub(crate) fn acquire(
        data_dir: &Path,
        repository: &Path,
        cancellation: &Cancellation,
    ) -> Result<Self, GitError> {
        let lock_dir = data_dir.join(LOCKS_DIRECTORY);
        let path = lock_path(&lock_dir, repository)?;
        fs::create_dir_all(&lock_dir).map_err(|source| GitError::Lock {
            path: lock_dir.clone(),
            source,
        })?;
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| GitError::Lock {
                path: path.clone(),
                source,
            })?;

        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        file,
                        #[cfg(test)]
                        path,
                    });
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(source)) => return Err(GitError::Lock { path, source }),
            }
            if cancellation.is_cancelled() {
                return Err(GitError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(GitError::RepositoryBusy {
                    path: repository.to_path_buf(),
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// The lock file this guard holds, inside the configured lock directory.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        // Closing the handle would release the lock on its own; unlocking
        // explicitly states where the critical section ends.
        let _ = self.file.unlock();
    }
}

/// Resolves the lock file shared by every worktree of one repository.
fn lock_path(lock_dir: &Path, repository: &Path) -> Result<PathBuf, GitError> {
    // `open` rather than `discover`: a path that is not itself a repository
    // must not be silently locked as its parent.
    let opened = match Repository::open(repository) {
        Ok(opened) => opened,
        Err(error) if error.code() == ErrorCode::NotFound => {
            return Err(GitError::NotARepository {
                path: repository.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(GitError::Inspection {
                path: repository.to_path_buf(),
                source: source.into(),
            });
        }
    };

    // The common directory is `.git` for a main worktree and the *parent's*
    // `.git` for a linked one, so both key the same lock. It is read in
    // process, with no spawn, because this runs before every mutation.
    let common_directory = opened.commondir();
    // Two aliases of one repository must not take two locks. An
    // uncanonicalizable path is used as it stands: failing to lock at all would
    // be a worse answer than locking under a name a symlinked alias might miss.
    let canonical =
        fs::canonicalize(common_directory).unwrap_or_else(|_| common_directory.to_path_buf());
    let identifier = Uuid::new_v5(
        &REPOSITORY_LOCK_NAMESPACE,
        canonical.as_os_str().as_encoded_bytes(),
    );
    Ok(lock_dir.join(format!("{identifier}.lock")))
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    use git2::WorktreeAddOptions;

    use super::RepositoryLock;
    use crate::{
        Cancellation, GitError, GitService,
        testing::{
            Fixture, PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV, initialize_repository,
            spawn_child, wait_for_child_signal,
        },
    };

    #[test]
    fn a_repository_that_is_not_one_cannot_be_locked() {
        let fixture = Fixture::new();
        let plain = fixture.directory("plain-directory");

        let error = RepositoryLock::acquire(&fixture.data_dir, &plain, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(error, GitError::NotARepository { .. }));
    }

    #[test]
    fn the_service_derives_the_stable_lock_directory_from_its_data_directory() {
        let fixture = Fixture::new();
        let repository = fixture.directory("locked-repository");
        initialize_repository(&repository);

        let session = GitService::new(&repository, &fixture.data_dir)
            .lock(&Cancellation::default())
            .unwrap();
        let lock = &session.lock;

        assert_eq!(
            lock.path().parent(),
            Some(fixture.data_dir.join("locks").as_path())
        );
        assert!(!lock.path().starts_with(&repository));
        assert!(
            !std::fs::read_dir(repository.join(".git"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".lock")),
            "locking wrote into the user's .git"
        );
    }

    /// Worktrees share `objects`, `refs` and `packed-refs`, so treating them as
    /// independent repositories would let two mutations run against one object
    /// store.
    #[test]
    fn every_worktree_of_a_repository_shares_one_lock() {
        let fixture = Fixture::new();
        let repository_root = fixture.directory("shared-lock-repository");
        let repository = initialize_repository(&repository_root);
        let worktree_root = fixture.root.path().join("shared-lock-worktree");
        repository
            .worktree("shared", &worktree_root, Some(&WorktreeAddOptions::new()))
            .unwrap();

        let held = RepositoryLock::acquire(
            &fixture.data_dir,
            &repository_root,
            &Cancellation::default(),
        )
        .unwrap();
        let contended =
            RepositoryLock::acquire(&fixture.data_dir, &worktree_root, &Cancellation::default())
                .unwrap_err();

        assert!(matches!(contended, GitError::RepositoryBusy { .. }));
        drop(held);
        // A linked worktree's `.git` is a file rather than a directory, so this
        // also pins that the lock does not require a `.git` directory.
        assert!(worktree_root.join(".git").is_file());
        RepositoryLock::acquire(&fixture.data_dir, &worktree_root, &Cancellation::default())
            .unwrap();
    }

    #[test]
    fn a_cancelled_wait_stops_before_the_busy_timeout() {
        let fixture = Fixture::new();
        let repository = fixture.directory("cancelled-lock-repository");
        initialize_repository(&repository);
        let _held =
            RepositoryLock::acquire(&fixture.data_dir, &repository, &Cancellation::default())
                .unwrap();

        let cancellation = Cancellation::default();
        cancellation.cancel();
        let error =
            RepositoryLock::acquire(&fixture.data_dir, &repository, &cancellation).unwrap_err();

        assert!(matches!(error, GitError::Cancelled));
    }

    /// Advisory locks are enforced by the kernel, so the contention that
    /// matters is between processes rather than between threads.
    #[test]
    fn a_second_process_is_told_the_repository_is_busy() {
        let fixture = Fixture::new();
        let repository = fixture.directory("cross-process-repository");
        initialize_repository(&repository);
        let ready_file = fixture.root.path().join("repository-lock-held");
        let mut child = spawn_child(&fixture.data_dir, "hold-repository-lock")
            .env(PROCESS_PROJECT_ROOT_ENV, &repository)
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();
        wait_for_child_signal(&mut child, &ready_file);

        let started = Instant::now();
        let error = GitService::new(&repository, &fixture.data_dir)
            .lock(&Cancellation::default())
            .unwrap_err();
        let waited = started.elapsed();

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(error, GitError::RepositoryBusy { ref path } if path == &repository),
            "{error}"
        );
        assert!(
            waited < Duration::from_secs(10),
            "the wait was not bounded: {waited:?}"
        );
        // The dead holder's lock is released by the kernel, so the repository
        // becomes workable again without any cleanup.
        wait_for_lock(&repository, &fixture.data_dir);
    }

    fn wait_for_lock(repository: &Path, data_dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match RepositoryLock::acquire(data_dir, repository, &Cancellation::default()) {
                Ok(_) => return,
                Err(error) if Instant::now() >= deadline => {
                    panic!("the repository stayed locked after its holder died: {error}")
                }
                Err(_) => {}
            }
        }
    }
}
