//! Fixtures shared by this crate's tests.

use std::{ops::Deref, path::Path, process::Command};

use harkness_git::{Cancellation, GitError, GitService, WorktreeBase};
use harkness_test_fixtures::Fixture as SharedFixture;
use harkness_test_fixtures::{child_path, park, signal_ready};
pub(crate) use harkness_test_fixtures::{
    commit_all, git, initialize_repository, wait_for_child_signal,
};

use crate::project::{ProjectError, ProjectService};

const PROCESS_CHILD_TEST: &str = "testing::process_child";
const PROCESS_ROLE_ENV: &str = "HARKNESS_CATALOG_TEST_ROLE";
const PROCESS_DATA_DIR_ENV: &str = "HARKNESS_CATALOG_TEST_DATA_DIR";
pub(crate) const PROCESS_PROJECT_ROOT_ENV: &str = "HARKNESS_CATALOG_TEST_PROJECT_ROOT";
pub(crate) const PROCESS_READY_FILE_ENV: &str = "HARKNESS_CATALOG_TEST_READY_FILE";
pub(crate) const PROCESS_PROJECT_ID_ENV: &str = "HARKNESS_CATALOG_TEST_PROJECT_ID";
pub(crate) const PROCESS_BRANCH_ENV: &str = "HARKNESS_CATALOG_TEST_BRANCH";

/// Re-entered by tests that need a distinct process boundary.
#[test]
#[ignore = "only run as a child process by catalog and repository locking tests"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let data_dir = child_path(PROCESS_DATA_DIR_ENV);
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();

    match role.as_str() {
        "import" => {
            service
                .import_local(child_path(PROCESS_PROJECT_ROOT_ENV))
                .unwrap();
        }
        "hold-lock" => {
            let _lock = service.lock_exclusive().unwrap();
            signal_ready(PROCESS_READY_FILE_ENV);
            park();
        }
        "load-env" => {
            // `load`, not `load_from_data_dir`: the environment alone must
            // redirect the platform data directory.
            let mut overridden = ProjectService::load().unwrap();
            assert_eq!(overridden.data_dir(), service.data_dir());
            overridden
                .import_local(child_path(PROCESS_PROJECT_ROOT_ENV))
                .unwrap();
        }
        "default-data-dir" => {
            assert_eq!(
                crate::paths::data_directory(),
                dirs::data_dir().map(|data_dir| data_dir.join("harkness"))
            );
        }
        "hold-repository-lock" => {
            let _lock = GitService::new(child_path(PROCESS_PROJECT_ROOT_ENV), &data_dir)
                .lock(&Cancellation::default())
                .unwrap();
            signal_ready(PROCESS_READY_FILE_ENV);
            park();
        }
        "create-worktree" => {
            let parent = std::env::var(PROCESS_PROJECT_ID_ENV)
                .expect("child parent id was not set")
                .parse()
                .expect("child parent id was invalid");
            let name = std::env::var(PROCESS_BRANCH_ENV).expect("child branch was not set");
            match service.create_worktree(
                parent,
                &WorktreeBase::NewBranch {
                    name,
                    start_point: None,
                },
                &Cancellation::default(),
            ) {
                Ok(_) | Err(ProjectError::Git(GitError::RepositoryBusy { .. })) => {}
                Err(error) => panic!("unexpected concurrent worktree result: {error}"),
            }
        }
        _ => panic!("unknown test child role: {role}"),
    }
}

pub(crate) fn spawn_child(data_dir: &Path, role: &str) -> Command {
    harkness_test_fixtures::spawn_child(
        PROCESS_CHILD_TEST,
        PROCESS_ROLE_ENV,
        role,
        PROCESS_DATA_DIR_ENV,
        data_dir,
    )
}

/// A shared fixture with a convenience constructor for the core service.
pub(crate) struct Fixture(SharedFixture);

impl Fixture {
    pub(crate) fn new() -> Self {
        Self(SharedFixture::new())
    }

    pub(crate) fn service(&self) -> ProjectService {
        ProjectService::load_for_test(&self.data_dir).unwrap()
    }
}

impl Deref for Fixture {
    type Target = SharedFixture;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
