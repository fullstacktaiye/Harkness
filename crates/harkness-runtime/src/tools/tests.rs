use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harkness_git::Cancellation;
use harkness_test_fixtures::{Fixture, initialize_repository};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::{ProcessExec, TestRun, register_mutating_tools};
use crate::domain::{RunId, StepId, ToolCallId};
use crate::tool::{
    ArtifactRef, ArtifactStream, ArtifactWriter, Deadline, DiscardedProgress, ExecutionContext,
    InvocationError, RiskLevel, Tool, ToolError, ToolRegistry, invoke,
};
use crate::tools::fs_apply_patch::{execute_with_after_write, execute_with_before_replace};
use crate::tools::{ApplyPatchInput, FileBase, FsApplyPatch};
use crate::trust::{BASELINE_ENVIRONMENT, EnvironmentName};

#[derive(Clone, Debug)]
struct DiskArtifacts {
    root: PathBuf,
    records: Arc<Mutex<BTreeMap<String, PathBuf>>>,
    next_id: Arc<AtomicUsize>,
}

impl DiskArtifacts {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            records: Arc::new(Mutex::new(BTreeMap::new())),
            next_id: Arc::new(AtomicUsize::new(1)),
        }
    }

    fn read(&self, name: &str) -> Vec<u8> {
        let path = self.records.lock().unwrap()[name].clone();
        fs::read(path).unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.records.lock().unwrap()[name].clone()
    }
}

impl ArtifactWriter for DiskArtifacts {
    fn open(&mut self, name: &str, media_type: &str) -> Result<Box<dyn ArtifactStream>, ToolError> {
        let id = format!("artifact-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let path = self.root.join(&id);
        let file = File::create(&path).map_err(ToolError::execution_failed)?;
        Ok(Box::new(DiskStream {
            file,
            path,
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            id,
            records: Arc::clone(&self.records),
            bytes: 0,
        }))
    }

    fn write_json(
        &mut self,
        name: &str,
        media_type: &str,
        value: &serde_json::Value,
    ) -> Result<ArtifactRef, ToolError> {
        let bytes = serde_json::to_vec(value).map_err(ToolError::execution_failed)?;
        self.write(name, media_type, &bytes)
    }
}

struct DiskStream {
    file: File,
    path: PathBuf,
    name: String,
    media_type: String,
    id: String,
    records: Arc<Mutex<BTreeMap<String, PathBuf>>>,
    bytes: u64,
}

impl Write for DiskStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.file.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl ArtifactStream for DiskStream {
    fn finish(mut self: Box<Self>) -> Result<ArtifactRef, ToolError> {
        self.file.flush().map_err(ToolError::execution_failed)?;
        self.file.sync_all().map_err(ToolError::execution_failed)?;
        self.records
            .lock()
            .unwrap()
            .insert(self.name.clone(), self.path.clone());
        Ok(ArtifactRef {
            id: self.id.clone(),
            media_type: self.media_type.clone(),
            byte_len: self.bytes,
        })
    }
}

struct Harness {
    workspace: TempDir,
    artifacts_root: TempDir,
    artifacts: DiskArtifacts,
}

impl Harness {
    fn new() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let artifacts_root = tempfile::tempdir().unwrap();
        let artifacts = DiskArtifacts::new(artifacts_root.path());
        Self {
            workspace,
            artifacts_root,
            artifacts,
        }
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            self.workspace.path().to_path_buf(),
            Cancellation::default(),
            Box::new(DiscardedProgress),
            Box::new(self.artifacts.clone()),
        )
        .unwrap()
        .with_deadline(Deadline::starting_now(Duration::from_secs(620)).unwrap())
    }

    fn invoke<T: Tool + 'static>(&self, tool: T, input: Value) -> Result<Value, InvocationError> {
        let mut registry = ToolRegistry::new();
        let identity = tool.metadata().identity().clone();
        registry.register(tool).unwrap();
        let raw = serde_json::value::to_raw_value(&input).unwrap();
        let mut context = self.context();
        invoke(
            &registry,
            &identity.id,
            Some(&identity.version),
            &raw,
            &mut context,
        )
        .map(|outcome| serde_json::from_str(outcome.output().get()).unwrap())
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn one_file_patch() -> &'static str {
    "diff --git a/tracked.txt b/tracked.txt\n\
     --- a/tracked.txt\n\
     +++ b/tracked.txt\n\
     @@ -1 +1 @@\n\
     -initial\n\
     +changed\n"
}

fn two_file_patch() -> &'static str {
    "diff --git a/tracked.txt b/tracked.txt\n\
     --- a/tracked.txt\n\
     +++ b/tracked.txt\n\
     @@ -1 +1 @@\n\
     -initial\n\
     +changed\n\
     diff --git a/z.txt b/z.txt\n\
     --- a/z.txt\n\
     +++ b/z.txt\n\
     @@ -1 +1 @@\n\
     -second\n\
     +also-changed\n"
}

fn two_file_input(first: &[u8], second: &[u8]) -> ApplyPatchInput {
    ApplyPatchInput {
        patch: two_file_patch().to_owned(),
        bases: vec![
            FileBase {
                path: "tracked.txt".to_owned(),
                base_sha256: Some(sha256(first)),
            },
            FileBase {
                path: "z.txt".to_owned(),
                base_sha256: Some(sha256(second)),
            },
        ],
    }
}

#[test]
fn empty_and_malformed_patches_are_conflicts_without_writes() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let before = fs::read(harness.workspace.path().join("tracked.txt")).unwrap();

    for patch in ["", "this is not a unified diff"] {
        let mut context = harness.context();
        let error = FsApplyPatch
            .execute(
                ApplyPatchInput {
                    patch: patch.to_owned(),
                    bases: vec![FileBase {
                        path: "tracked.txt".to_owned(),
                        base_sha256: Some(sha256(&before)),
                    }],
                },
                &mut context,
            )
            .unwrap_err();
        assert_eq!(error.kind(), "patch_conflict");
        assert_eq!(
            fs::read(harness.workspace.path().join("tracked.txt")).unwrap(),
            before
        );
    }
}

#[test]
fn a_new_file_with_spaces_unicode_and_no_final_newline_is_created_byte_exactly() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let relative = "notes/a spaced 資料.txt";
    fs::create_dir(harness.workspace.path().join("notes")).unwrap();
    let patch = "diff --git \"a/notes/a spaced 資料.txt\" \"b/notes/a spaced 資料.txt\"\n\
                 new file mode 100644\n\
                 --- /dev/null\n\
                 +++ \"b/notes/a spaced 資料.txt\"\n\
                 @@ -0,0 +1 @@\n\
                 +without newline\n\
                 \\ No newline at end of file\n";
    let output = harness
        .invoke(
            FsApplyPatch,
            json!({
                "patch": patch,
                "bases": [{"path": relative, "base_sha256": null}]
            }),
        )
        .unwrap();
    assert_eq!(
        fs::read(harness.workspace.path().join(relative)).unwrap(),
        b"without newline"
    );
    assert_eq!(output["files"][0]["path"], relative);
    assert_eq!(output["files"][0]["change"], "created");
}

#[test]
fn built_ins_register_at_version_one_with_honest_risk_and_process_metadata() {
    let mut registry = ToolRegistry::new();
    register_mutating_tools(&mut registry).unwrap();
    let descriptors = registry.descriptors().collect::<Vec<_>>();
    assert_eq!(
        descriptors
            .iter()
            .map(|descriptor| descriptor.identity().to_string())
            .collect::<Vec<_>>(),
        [
            "check.run@1.0.0",
            "fs.apply_patch@1.0.0",
            "process.exec@1.0.0",
            "test.run@1.0.0"
        ]
    );
    for descriptor in descriptors {
        if descriptor.id().as_str() == "fs.apply_patch" {
            assert_eq!(descriptor.risk(), RiskLevel::WorkspaceWrite);
            assert!(!descriptor.spawns_processes());
        } else {
            assert_eq!(descriptor.risk(), RiskLevel::Execute);
            assert!(descriptor.spawns_processes());
            assert_eq!(descriptor.capabilities()[0].as_str(), "process.spawn");
        }
    }
}

#[test]
fn applying_a_matching_patch_atomically_returns_the_resulting_diff_artifact() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let original = fs::read(harness.workspace.path().join("tracked.txt")).unwrap();
    let output = harness
        .invoke(
            FsApplyPatch,
            json!({
                "patch": one_file_patch(),
                "bases": [{"path": "tracked.txt", "base_sha256": sha256(&original)}]
            }),
        )
        .unwrap();

    assert_eq!(
        fs::read(harness.workspace.path().join("tracked.txt")).unwrap(),
        b"changed\n"
    );
    assert_eq!(output["files"][0]["change"], "modified");
    assert_eq!(output["files"][0]["hunks_applied"], 1);
    assert_eq!(output["files"][0]["byte_delta"], 0);
    let expected =
        harkness_test_fixtures::git(harness.workspace.path(), ["diff", "--", "tracked.txt"]);
    assert_eq!(
        harness.artifacts.read("applied.patch"),
        expected.as_bytes(),
        "the side artifact must be the actual Git worktree diff"
    );
}

#[test]
fn a_stale_base_refuses_before_writing_any_file() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let path = harness.workspace.path().join("tracked.txt");
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: one_file_patch().to_owned(),
                bases: vec![FileBase {
                    path: "tracked.txt".to_owned(),
                    base_sha256: Some("0".repeat(64)),
                }],
            },
            &mut context,
        )
        .unwrap_err();
    assert_eq!(error.kind(), "stale_patch");
    assert_eq!(fs::read(path).unwrap(), b"initial\n");
}

#[test]
fn a_later_target_changed_during_commit_is_not_overwritten() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let first = fs::read(harness.workspace.path().join("tracked.txt")).unwrap();
    let second_path = harness.workspace.path().join("z.txt");
    fs::write(&second_path, b"second\n").unwrap();
    let second = fs::read(&second_path).unwrap();
    let mut context = harness.context();

    let error =
        execute_with_after_write(two_file_input(&first, &second), &mut context, |written| {
            if written == Path::new("tracked.txt") {
                fs::write(&second_path, b"external edit\n").unwrap();
            }
        })
        .unwrap_err();

    assert_eq!(error.kind(), "stale_patch");
    assert_eq!(fs::read(second_path).unwrap(), b"external edit\n");
}

#[test]
fn cancellation_after_commit_starts_finishes_the_validated_batch() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let first = fs::read(harness.workspace.path().join("tracked.txt")).unwrap();
    let second_path = harness.workspace.path().join("z.txt");
    fs::write(&second_path, b"second\n").unwrap();
    let second = fs::read(&second_path).unwrap();
    let mut context = harness.context();
    let cancellation = context.cancellation().clone();

    let output =
        execute_with_after_write(two_file_input(&first, &second), &mut context, |written| {
            if written == Path::new("tracked.txt") {
                cancellation.cancel();
            }
        })
        .unwrap();

    assert_eq!(output.files.len(), 2);
    assert_eq!(fs::read(second_path).unwrap(), b"also-changed\n");
    assert!(cancellation.is_cancelled());
}

#[test]
fn git_administration_paths_are_never_workspace_patch_targets() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let head = harness.workspace.path().join(".git/HEAD");
    let original = fs::read(&head).unwrap();
    let patch = "diff --git a/.git/HEAD b/.git/HEAD\n\
                 --- a/.git/HEAD\n\
                 +++ b/.git/HEAD\n\
                 @@ -1 +1 @@\n\
                 -ref: refs/heads/main\n\
                 +ref: refs/heads/other\n";
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![FileBase {
                    path: ".git/HEAD".to_owned(),
                    base_sha256: Some(sha256(&original)),
                }],
            },
            &mut context,
        )
        .unwrap_err();

    assert_eq!(error.kind(), "forbidden_path");
    assert_eq!(fs::read(head).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn a_contained_symlink_is_refused_instead_of_rewriting_its_target() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let target = harness.workspace.path().join("tracked.txt");
    symlink("tracked.txt", harness.workspace.path().join("alias.txt")).unwrap();
    let original = fs::read(&target).unwrap();
    let patch = one_file_patch().replace("tracked.txt", "alias.txt");
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch,
                bases: vec![FileBase {
                    path: "alias.txt".to_owned(),
                    base_sha256: Some(sha256(&original)),
                }],
            },
            &mut context,
        )
        .unwrap_err();

    assert_eq!(error.kind(), "forbidden_path");
    assert_eq!(fs::read(target).unwrap(), original);
}

#[test]
fn differing_old_and_new_paths_are_rejected_as_a_rename() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let target = harness.workspace.path().join("tracked.txt");
    let original = fs::read(&target).unwrap();
    let patch = "diff --git a/old.txt b/tracked.txt\n\
                 --- a/old.txt\n\
                 +++ b/tracked.txt\n\
                 @@ -1 +1 @@\n\
                 -initial\n\
                 +changed\n";
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![FileBase {
                    path: "tracked.txt".to_owned(),
                    base_sha256: Some(sha256(&original)),
                }],
            },
            &mut context,
        )
        .unwrap_err();

    assert_eq!(error.kind(), "patch_conflict");
    assert_eq!(fs::read(target).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn an_executable_mode_change_is_applied_with_the_content() {
    use std::os::unix::fs::PermissionsExt;

    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let path = harness.workspace.path().join("tracked.txt");
    let original = fs::read(&path).unwrap();
    let patch = "diff --git a/tracked.txt b/tracked.txt\n\
                 old mode 100644\n\
                 new mode 100755\n\
                 index e79c5e8..5ea2ed4\n\
                 --- a/tracked.txt\n\
                 +++ b/tracked.txt\n\
                 @@ -1 +1 @@\n\
                 -initial\n\
                 +changed\n";
    let mut context = harness.context();
    FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![FileBase {
                    path: "tracked.txt".to_owned(),
                    base_sha256: Some(sha256(&original)),
                }],
            },
            &mut context,
        )
        .unwrap();

    assert_ne!(fs::metadata(path).unwrap().permissions().mode() & 0o111, 0);
}

#[test]
fn one_conflicting_hunk_refuses_a_multi_file_patch_without_partial_writes() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let first = harness.workspace.path().join("tracked.txt");
    let second = harness.workspace.path().join("second.txt");
    fs::write(&second, b"second\n").unwrap();
    let first_base = fs::read(&first).unwrap();
    let second_base = fs::read(&second).unwrap();
    let patch = "diff --git a/tracked.txt b/tracked.txt\n\
                 --- a/tracked.txt\n\
                 +++ b/tracked.txt\n\
                 @@ -1 +1 @@\n\
                 -initial\n\
                 +changed\n\
                 diff --git a/second.txt b/second.txt\n\
                 --- a/second.txt\n\
                 +++ b/second.txt\n\
                 @@ -1 +1 @@\n\
                 -not-the-base\n\
                 +also-changed\n";
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![
                    FileBase {
                        path: "tracked.txt".to_owned(),
                        base_sha256: Some(sha256(&first_base)),
                    },
                    FileBase {
                        path: "second.txt".to_owned(),
                        base_sha256: Some(sha256(&second_base)),
                    },
                ],
            },
            &mut context,
        )
        .unwrap_err();
    assert_eq!(error.kind(), "patch_conflict");
    assert_eq!(fs::read(first).unwrap(), first_base);
    assert_eq!(fs::read(second).unwrap(), second_base);
}

#[cfg(unix)]
#[test]
fn a_patch_targeting_an_escaping_symlink_is_forbidden_without_writing() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret\n").unwrap();
    symlink(outside.path(), harness.workspace.path().join("escape")).unwrap();
    let patch = "diff --git a/escape/secret.txt b/escape/secret.txt\n\
                 --- a/escape/secret.txt\n\
                 +++ b/escape/secret.txt\n\
                 @@ -1 +1 @@\n\
                 -secret\n\
                 +leaked\n";
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![FileBase {
                    path: "escape/secret.txt".to_owned(),
                    base_sha256: Some(sha256(b"secret\n")),
                }],
            },
            &mut context,
        )
        .unwrap_err();
    assert_eq!(error.kind(), "forbidden_path");
    assert_eq!(
        fs::read(outside.path().join("secret.txt")).unwrap(),
        b"secret\n"
    );
}

#[test]
fn mandatory_diff_artifact_is_opened_before_any_patch_write() {
    struct RefusingArtifacts;

    impl ArtifactWriter for RefusingArtifacts {
        fn open(
            &mut self,
            _name: &str,
            _media_type: &str,
        ) -> Result<Box<dyn ArtifactStream>, ToolError> {
            Err(ToolError::execution_failed("artifact storage unavailable"))
        }

        fn write_json(
            &mut self,
            _name: &str,
            _media_type: &str,
            _value: &serde_json::Value,
        ) -> Result<ArtifactRef, ToolError> {
            Err(ToolError::execution_failed("artifact storage unavailable"))
        }
    }

    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let target = harness.workspace.path().join("tracked.txt");
    let original = fs::read(&target).unwrap();
    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        harness.workspace.path().to_path_buf(),
        Cancellation::default(),
        Box::new(DiscardedProgress),
        Box::new(RefusingArtifacts),
    )
    .unwrap();

    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: one_file_patch().to_owned(),
                bases: vec![FileBase {
                    path: "tracked.txt".to_owned(),
                    base_sha256: Some(sha256(&original)),
                }],
            },
            &mut context,
        )
        .unwrap_err();

    assert_eq!(error.kind(), "execution_failed");
    assert_eq!(fs::read(target).unwrap(), original);
}

#[test]
fn a_target_changed_immediately_before_replacement_is_not_overwritten() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let target = harness.workspace.path().join("tracked.txt");
    let original = fs::read(&target).unwrap();
    let mut context = harness.context();

    let error = execute_with_before_replace(
        ApplyPatchInput {
            patch: one_file_patch().to_owned(),
            bases: vec![FileBase {
                path: "tracked.txt".to_owned(),
                base_sha256: Some(sha256(&original)),
            }],
        },
        &mut context,
        |_| fs::write(&target, b"external edit\n").unwrap(),
    )
    .unwrap_err();

    assert_eq!(error.kind(), "stale_patch");
    assert_eq!(fs::read(target).unwrap(), b"external edit\n");
}

#[cfg(unix)]
#[test]
fn an_ancestor_swapped_to_an_outside_symlink_before_replacement_is_refused() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let parent = harness.workspace.path().join("nested");
    let displaced = harness.workspace.path().join("displaced");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("tracked.txt"), b"initial\n").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("tracked.txt"), b"outside\n").unwrap();
    let patch = one_file_patch().replace("tracked.txt", "nested/tracked.txt");
    let mut context = harness.context();

    let error = execute_with_before_replace(
        ApplyPatchInput {
            patch,
            bases: vec![FileBase {
                path: "nested/tracked.txt".to_owned(),
                base_sha256: Some(sha256(b"initial\n")),
            }],
        },
        &mut context,
        |_| {
            fs::rename(&parent, &displaced).unwrap();
            symlink(outside.path(), &parent).unwrap();
        },
    )
    .unwrap_err();

    assert_eq!(error.kind(), "forbidden_path");
    assert_eq!(
        fs::read(outside.path().join("tracked.txt")).unwrap(),
        b"outside\n"
    );
    assert_eq!(
        fs::read(displaced.join("tracked.txt")).unwrap(),
        b"initial\n"
    );
}

#[cfg(windows)]
#[test]
fn windows_normalized_git_aliases_are_never_patch_targets() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    for alias in [".git ", ".git.", ".GIT "] {
        let patch = one_file_patch().replace("tracked.txt", &format!("{alias}/HEAD"));
        let mut context = harness.context();
        let error = FsApplyPatch
            .execute(
                ApplyPatchInput {
                    patch,
                    bases: vec![FileBase {
                        path: format!("{alias}/HEAD"),
                        base_sha256: None,
                    }],
                },
                &mut context,
            )
            .unwrap_err();
        assert_eq!(error.kind(), "forbidden_path", "alias {alias:?}");
    }
}

#[cfg(windows)]
#[test]
fn windows_platform_equivalent_targets_are_rejected_before_writing() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    fs::write(harness.workspace.path().join("name.txt"), b"one\n").unwrap();
    let patch = "diff --git a/name.txt b/name.txt\n\
                 --- a/name.txt\n\
                 +++ b/name.txt\n\
                 @@ -1 +1 @@\n\
                 -one\n\
                 +first\n\
                 diff --git a/NAME.TXT b/NAME.TXT\n\
                 --- a/NAME.TXT\n\
                 +++ b/NAME.TXT\n\
                 @@ -1 +1 @@\n\
                 -one\n\
                 +second\n";
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![
                    FileBase {
                        path: "name.txt".to_owned(),
                        base_sha256: Some(sha256(b"one\n")),
                    },
                    FileBase {
                        path: "NAME.TXT".to_owned(),
                        base_sha256: Some(sha256(b"one\n")),
                    },
                ],
            },
            &mut context,
        )
        .unwrap_err();
    assert_eq!(error.kind(), "patch_conflict");
    assert_eq!(
        fs::read(harness.workspace.path().join("name.txt")).unwrap(),
        b"one\n"
    );
}

#[cfg(not(unix))]
#[test]
fn explicit_executable_mode_is_refused_where_it_cannot_be_applied() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let original = fs::read(harness.workspace.path().join("tracked.txt")).unwrap();
    let patch = "diff --git a/tracked.txt b/tracked.txt\n\
                 old mode 100644\n\
                 new mode 100755\n\
                 --- a/tracked.txt\n\
                 +++ b/tracked.txt\n\
                 @@ -1 +1 @@\n\
                 -initial\n\
                 +changed\n";
    let mut context = harness.context();
    let error = FsApplyPatch
        .execute(
            ApplyPatchInput {
                patch: patch.to_owned(),
                bases: vec![FileBase {
                    path: "tracked.txt".to_owned(),
                    base_sha256: Some(sha256(&original)),
                }],
            },
            &mut context,
        )
        .unwrap_err();
    assert_eq!(error.kind(), "patch_conflict");
    assert_eq!(
        fs::read(harness.workspace.path().join("tracked.txt")).unwrap(),
        original
    );
}

#[cfg(unix)]
#[test]
fn process_exec_preserves_shell_metacharacters_as_single_arguments() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let shim = fixture.shim("argv", "#!/bin/sh\nprintf '<%s>\\n' \"$@\"\n");
    let output = harness
        .invoke(
            ProcessExec,
            json!({
                "argv": [shim, "; rm -rf workspace", "$(touch impossible)"],
                "timeout_seconds": 5
            }),
        )
        .unwrap();
    assert_eq!(output["exit_code"], 0);
    assert!(!output["timed_out"].as_bool().unwrap());
    assert_eq!(
        harness.artifacts.read("process-stdout.log"),
        b"<; rm -rf workspace>\n<$(touch impossible)>\n"
    );
    assert!(!harness.workspace.path().join("impossible").exists());
}

#[test]
fn cancellation_during_artifact_setup_prevents_process_launch() {
    struct CancellingArtifacts {
        inner: DiskArtifacts,
        cancellation: Cancellation,
        opened: usize,
    }

    impl ArtifactWriter for CancellingArtifacts {
        fn open(
            &mut self,
            name: &str,
            media_type: &str,
        ) -> Result<Box<dyn ArtifactStream>, ToolError> {
            self.opened += 1;
            if self.opened == 2 {
                self.cancellation.cancel();
            }
            self.inner.open(name, media_type)
        }

        fn write_json(
            &mut self,
            name: &str,
            media_type: &str,
            value: &serde_json::Value,
        ) -> Result<ArtifactRef, ToolError> {
            self.inner.write_json(name, media_type, value)
        }
    }

    let harness = Harness::new();
    let cancellation = Cancellation::default();
    let artifacts = CancellingArtifacts {
        inner: harness.artifacts.clone(),
        cancellation: cancellation.clone(),
        opened: 0,
    };
    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        harness.workspace.path().to_path_buf(),
        cancellation,
        Box::new(DiscardedProgress),
        Box::new(artifacts),
    )
    .unwrap();
    let mut registry = ToolRegistry::new();
    let identity = ProcessExec.metadata().identity().clone();
    registry.register(ProcessExec).unwrap();
    let input = serde_json::value::to_raw_value(&json!({
        "argv": ["harkness-command-that-must-never-be-spawned"]
    }))
    .unwrap();

    let error = invoke(
        &registry,
        &identity.id,
        Some(&identity.version),
        &input,
        &mut context,
    )
    .unwrap_err();

    assert_eq!(error.kind(), "cancelled");
}

#[cfg(unix)]
#[test]
fn process_timeout_is_capped_and_returns_a_typed_killed_result() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let shim = fixture.shim("long-running", "#!/bin/sh\nexec sleep 30\n");
    let output = harness
        .invoke(ProcessExec, json!({"argv": [shim], "timeout_seconds": 1}))
        .unwrap();
    assert_eq!(output["timed_out"], true);
    assert_eq!(output["timeout_seconds"], 1);
    assert!(output["exit_code"].is_null());
    assert_eq!(output["signal"], libc::SIGKILL);
    assert!(output["duration_ms"].as_u64().unwrap() >= 1_000);
}

#[cfg(target_os = "linux")]
#[test]
fn process_timeout_kills_a_spawned_grandchild_too() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let pid_file = harness.workspace.path().join("grandchild.pid");
    let shim = fixture.shim(
        "spawns-grandchild",
        "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > \"$1\"\n\nwait\n",
    );
    let output = harness
        .invoke(
            ProcessExec,
            json!({"argv": [shim, pid_file], "timeout_seconds": 1}),
        )
        .unwrap();
    assert_eq!(output["timed_out"], true);
    let pid = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    while linux_process_is_live(pid) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        !linux_process_is_live(pid),
        "grandchild {pid} survived the process-group timeout"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_detached_session_cannot_hold_a_timed_out_call_open() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let pid_file = harness.workspace.path().join("detached.pid");
    let shim = fixture.shim(
        "spawns-detached-session",
        "#!/bin/sh\nsetsid sh -c 'printf \"%s\\n\" \"$$\" > \"$1\"; sleep 30' sh \"$1\" &\nwait\n",
    );
    let started = std::time::Instant::now();
    let output = harness
        .invoke(
            ProcessExec,
            json!({"argv": [shim, pid_file], "timeout_seconds": 1}),
        )
        .unwrap();
    assert_eq!(output["timed_out"], true);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a detached writer kept the output readers alive"
    );
    let pid = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<libc::pid_t>()
        .unwrap();
    assert!(
        linux_process_is_live(u32::try_from(pid).unwrap()),
        "the regression must actually escape the supervised process group"
    );
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(windows)]
#[test]
fn process_timeout_terminates_a_windows_descendant_job() {
    let harness = Harness::new();
    let pid_file = harness.workspace.path().join("grandchild.pid");
    let executable = std::env::current_exe().unwrap();
    let output = harness
        .invoke(
            ProcessExec,
            json!({
                "argv": [
                    executable,
                    "--ignored",
                    "--exact",
                    "tools::tests::windows_descendant_fixture",
                    "--nocapture"
                ],
                "timeout_seconds": 2
            }),
        )
        .unwrap();
    assert_eq!(output["timed_out"], true);
    let pid = fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    while windows_process_is_live(pid) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        !windows_process_is_live(pid),
        "grandchild {pid} survived the Job Object timeout"
    );
}

#[cfg(windows)]
#[test]
#[ignore = "only run as a child process by the Windows Job Object regression"]
fn windows_descendant_fixture() {
    let executable = std::env::current_exe().unwrap();
    let child = std::process::Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "tools::tests::windows_sleep_fixture",
            "--nocapture",
        ])
        .spawn()
        .unwrap();
    fs::write("grandchild.pid", child.id().to_string()).unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

#[cfg(windows)]
#[test]
#[ignore = "only run as a child process by the Windows Job Object fixture"]
fn windows_sleep_fixture() {
    std::thread::sleep(Duration::from_secs(30));
}

#[cfg(windows)]
fn windows_process_is_live(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    let read = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    read && exit_code == STILL_ACTIVE as u32
}

#[cfg(target_os = "linux")]
fn linux_process_is_live(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, fields)| fields.as_bytes().first())
        .is_some_and(|state| *state != b'Z')
}

#[cfg(unix)]
#[test]
fn process_exec_does_not_inherit_an_undeclared_parent_canary() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let canary = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .find(|name| {
            EnvironmentName::new(name)
                .is_ok_and(|name| !BASELINE_ENVIRONMENT.contains(&name.as_str()))
        })
        .expect("Cargo's test process carries at least one non-baseline variable");
    let shim = fixture.shim(
        "env-canary",
        &format!(
            "#!/bin/sh\nif [ \"${{{canary}+present}}\" = present ]; then printf leaked; else printf absent; fi\n"
        ),
    );
    let output = harness
        .invoke(ProcessExec, json!({"argv": [shim]}))
        .unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(harness.artifacts.read("process-stdout.log"), b"absent");
}

#[cfg(unix)]
#[test]
fn fifty_megabytes_stream_to_disk_while_only_a_small_tail_is_returned() {
    const BYTES: u64 = 50 * 1024 * 1024;
    let harness = Harness::new();
    let fixture = Fixture::new();
    let shim = fixture.shim(
        "large-output",
        &format!("#!/bin/sh\nyes x | head -c {BYTES}\n"),
    );
    let output = harness
        .invoke(ProcessExec, json!({"argv": [shim], "timeout_seconds": 30}))
        .unwrap();
    assert_eq!(output["stdout_tail"]["byte_len"], BYTES);
    assert_eq!(output["stdout_tail"]["truncated"], true);
    assert!(output["stdout_tail"]["text"].as_str().unwrap().len() <= 4 * 1024);
    assert_eq!(
        fs::metadata(harness.artifacts.path("process-stdout.log"))
            .unwrap()
            .len(),
        BYTES
    );
    // Keep the directory owner live until after the metadata assertion.
    assert!(harness.artifacts_root.path().exists());
}

#[cfg(unix)]
#[test]
fn test_run_reports_pass_and_failure_without_turning_a_failed_test_into_a_tool_failure() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let passing = fixture.shim("passing", "#!/bin/sh\necho passed\n");
    let passed = harness
        .invoke(TestRun, json!({"command": [passing]}))
        .unwrap();
    assert_eq!(passed["passed"], true);
    assert_eq!(passed["exit_code"], 0);

    let failing = fixture.shim("failing", "#!/bin/sh\necho failed >&2\nexit 7\n");
    let failed = harness
        .invoke(TestRun, json!({"command": [failing]}))
        .unwrap();
    assert_eq!(failed["passed"], false);
    assert_eq!(failed["exit_code"], 7);
    assert_eq!(harness.artifacts.read("test-stderr.log"), b"failed\n");
}

#[test]
fn empty_argv_is_rejected_by_schema_before_the_process_body() {
    let harness = Harness::new();
    let error = harness
        .invoke(ProcessExec, json!({"argv": []}))
        .unwrap_err();
    assert_eq!(error.kind(), "invalid_input");
}

#[cfg(unix)]
#[test]
fn an_environment_override_not_published_by_the_descriptor_is_refused_before_spawn() {
    let harness = Harness::new();
    let fixture = Fixture::new();
    let marker = harness.workspace.path().join("started");
    let shim = fixture.shim(
        "must-not-start",
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    let error = harness
        .invoke(
            ProcessExec,
            json!({
                "argv": [shim],
                "env": {"HARKNESS_SECRET_CANARY": "must-not-leak"}
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "execution_failed");
    assert!(
        !marker.exists(),
        "the child must not be spawned on env refusal"
    );
}
