//! Fixtures shared by this crate's tests.

use std::{fs, path::Path, process::Command};

pub(crate) use harkness_test_fixtures::{
    COMMIT_EPOCH_SECONDS, Fixture, commit_all, configure_commit_identity, git,
    initialize_repository, remote_with_clone, wait_for_child_signal, wait_for_file,
};
use harkness_test_fixtures::{child_path, park, signal_ready};

use crate::{Cancellation, CommitOptions, GitService};

const PROCESS_CHILD_TEST: &str = "testing::process_child";
const PROCESS_ROLE_ENV: &str = "HARKNESS_GIT_TEST_ROLE";
const PROCESS_DATA_DIR_ENV: &str = "HARKNESS_GIT_TEST_DATA_DIR";
pub(crate) const PROCESS_PROJECT_ROOT_ENV: &str = "HARKNESS_GIT_TEST_PROJECT_ROOT";
pub(crate) const PROCESS_READY_FILE_ENV: &str = "HARKNESS_GIT_TEST_READY_FILE";
pub(crate) const PROCESS_GIT_EXECUTABLE_ENV: &str = "HARKNESS_GIT_TEST_EXECUTABLE";

/// The inherited variables the runner must remove before spawning Git.
pub(crate) const SCRUBBED_ENVIRONMENT: [&str; 13] = [
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
];

#[test]
#[ignore = "only run as a child process by Git locking and runner tests"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let data_dir = child_path(PROCESS_DATA_DIR_ENV);

    match role.as_str() {
        "hold-repository-lock" => {
            let _repository = GitService::new(child_path(PROCESS_PROJECT_ROOT_ENV), &data_dir)
                .lock(&Cancellation::default())
                .unwrap();
            signal_ready(PROCESS_READY_FILE_ENV);
            park();
        }
        "commit-with-isolated-config" => {
            let root = child_path(PROCESS_PROJECT_ROOT_ENV);
            initialize_repository(&root);
            fs::write(root.join("tracked.txt"), "committed through system Git\n").unwrap();
            let git = GitService::new(&root, &data_dir);
            git.stage(["tracked.txt"], &Cancellation::default())
                .unwrap();
            git.commit(
                "isolated fixture commit",
                &CommitOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        }
        "scrubbed-environment" => {
            for name in SCRUBBED_ENVIRONMENT {
                assert!(
                    std::env::var_os(name).is_some(),
                    "the parent did not export {name}, so this proves nothing"
                );
            }
            let git_executable = child_path(PROCESS_GIT_EXECUTABLE_ENV);
            let root = child_path(PROCESS_PROJECT_ROOT_ENV);
            initialize_repository(&root);
            GitService::new(&root, &data_dir)
                .with_git_executable(git_executable)
                .worktrees(&Cancellation::default())
                .unwrap();
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
