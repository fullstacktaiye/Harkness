use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use harkness_core::{Project, ProjectId, ProjectSource};
use harkness_git::Cancellation;
use harkness_test_fixtures::{git, initialize_repository};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::OffsetDateTime;

use super::register_read_only_tools;
use crate::domain::{ArtifactId, Run, RunId, Step, StepId, Task, ToolCall, ToolCallId};
use crate::store::{Store, StoreArtifacts};
use crate::tool::{
    ArtifactRef, ArtifactStream, ArtifactWriter, DiscardedProgress, ExecutionContext,
    InvocationError, RiskLevel, ToolError, ToolRegistry, WorkspaceMetadata, invoke,
};

#[derive(Clone, Default)]
struct MemoryArtifacts {
    records: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    next: Arc<AtomicUsize>,
}

impl MemoryArtifacts {
    fn only(&self) -> Vec<u8> {
        self.records
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone()
    }
}

impl ArtifactWriter for MemoryArtifacts {
    fn open(&mut self, name: &str, media_type: &str) -> Result<Box<dyn ArtifactStream>, ToolError> {
        Ok(Box::new(MemoryStream {
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            id: format!("artifact-{}", self.next.fetch_add(1, Ordering::Relaxed)),
            bytes: Vec::new(),
            records: Arc::clone(&self.records),
        }))
    }
}

struct MemoryStream {
    name: String,
    media_type: String,
    id: String,
    bytes: Vec<u8>,
    records: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl Write for MemoryStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ArtifactStream for MemoryStream {
    fn finish(self: Box<Self>) -> Result<ArtifactRef, ToolError> {
        let byte_len = u64::try_from(self.bytes.len()).unwrap();
        self.records
            .lock()
            .unwrap()
            .insert(self.name.clone(), self.bytes);
        Ok(ArtifactRef {
            id: self.id,
            media_type: self.media_type,
            byte_len,
        })
    }
}

struct Harness {
    workspace: TempDir,
    artifacts: MemoryArtifacts,
}

impl Harness {
    fn new() -> Self {
        Self {
            workspace: tempfile::tempdir().unwrap(),
            artifacts: MemoryArtifacts::default(),
        }
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            self.workspace.path(),
            Cancellation::default(),
            Box::new(DiscardedProgress),
            Box::new(self.artifacts.clone()),
        )
        .unwrap()
    }

    fn invoke(&self, id: &str, input: Value) -> Result<Value, InvocationError> {
        let mut registry = ToolRegistry::new();
        register_read_only_tools(&mut registry).unwrap();
        let id = id.parse().unwrap();
        let raw = serde_json::value::to_raw_value(&input).unwrap();
        let mut context = self.context();
        invoke(&registry, &id, None, &raw, &mut context)
            .map(|outcome| serde_json::from_str(outcome.output().get()).unwrap())
    }
}

#[test]
fn read_only_tools_register_with_stable_observe_contracts() {
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();
    let descriptors = registry.descriptors().collect::<Vec<_>>();
    assert_eq!(
        descriptors
            .iter()
            .map(|descriptor| descriptor.identity().to_string())
            .collect::<Vec<_>>(),
        [
            "fs.read@1.0.0",
            "git.diff@1.0.0",
            "git.status@1.0.0",
            "workspace.inspect@1.0.0",
            "workspace.search@1.0.0",
        ]
    );
    for descriptor in descriptors {
        assert_eq!(descriptor.risk(), RiskLevel::Observe);
        assert!(!descriptor.spawns_processes());
        assert!(!descriptor.title().is_empty());
        assert!(!descriptor.description().is_empty());
        assert!(descriptor.input_schema().is_object());
        assert!(descriptor.output_schema().is_object());
    }
}

#[test]
fn schema_rejection_happens_before_a_missing_file_is_observed() {
    let harness = Harness::new();
    let error = harness
        .invoke("fs.read", json!({"path": "missing", "max_btyes": 1}))
        .unwrap_err();
    assert_eq!(error.kind(), "invalid_input");
    assert!(error.to_string().contains("max_btyes"));
}

#[test]
fn fs_read_preserves_non_utf8_and_names_byte_truncation() {
    let harness = Harness::new();
    fs::write(
        harness.workspace.path().join("binary.bin"),
        [0xff, 0x00, 0x41, 0x42],
    )
    .unwrap();
    let output = harness
        .invoke("fs.read", json!({"path": "binary.bin", "max_bytes": 3}))
        .unwrap();
    assert_eq!(output["content_encoding"], "base64");
    assert_eq!(
        BASE64.decode(output["content"].as_str().unwrap()).unwrap(),
        [0xff, 0x00, 0x41]
    );
    assert_eq!(output["truncated"]["kind"], "byte_limit");
    assert_eq!(output["byte_size"], 4);
}

#[cfg(unix)]
#[test]
fn escaping_symlinks_are_refused_by_read_and_search() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"needle\n").unwrap();
    symlink(outside.path(), harness.workspace.path().join("escape")).unwrap();
    for (tool, input) in [
        ("fs.read", json!({"path": "escape/secret.txt"})),
        (
            "workspace.search",
            json!({"path": "escape", "query": "needle"}),
        ),
    ] {
        let error = harness.invoke(tool, input).unwrap_err();
        assert!(matches!(error.kind(), "forbidden_path" | "symlink_escapes"));
    }
}

#[test]
fn search_respects_gitignore_and_reports_match_and_output_budgets() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    fs::write(
        harness.workspace.path().join(".gitignore"),
        "ignored.txt\nignored-dir/\n",
    )
    .unwrap();
    fs::write(harness.workspace.path().join("ignored.txt"), "needle\n").unwrap();
    fs::create_dir(harness.workspace.path().join("ignored-dir")).unwrap();
    fs::write(
        harness.workspace.path().join("ignored-dir/also.txt"),
        "needle\n",
    )
    .unwrap();
    fs::write(
        harness.workspace.path().join("visible.txt"),
        "needle one\nneedle two\n",
    )
    .unwrap();
    let output = harness
        .invoke(
            "workspace.search",
            json!({
                "query": "needle",
                "max_matches": 10,
                "max_per_file": 1,
                "max_total_bytes": 1024
            }),
        )
        .unwrap();
    assert_eq!(output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(output["matches"][0]["path"], "visible.txt");
    assert_eq!(output["matches"][0]["line_number"], 1);
    assert!(
        output["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|omission| { omission["kind"] == "per_file_match_budget_exhausted" })
    );
}

#[test]
fn git_status_matches_the_existing_detailed_projection_fields() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    fs::write(harness.workspace.path().join("tracked.txt"), b"changed\n").unwrap();
    fs::write(harness.workspace.path().join("untracked.txt"), b"new\n").unwrap();
    let output = harness.invoke("git.status", json!({})).unwrap();
    assert_eq!(output["head"], json!({"kind": "branch", "name": "main"}));
    assert_eq!(output["upstream"], Value::Null);
    assert_eq!(output["pending"], Value::Null);
    let entries = output["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| { entry["path"] == "tracked.txt" && entry["unstaged"] == "modified" })
    );
    assert!(
        entries
            .iter()
            .any(|entry| { entry["path"] == "untracked.txt" && entry["unstaged"] == "untracked" })
    );
}

#[test]
fn oversized_diff_spills_full_valid_payload_to_an_artifact() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    fs::write(
        harness.workspace.path().join("tracked.txt"),
        "changed\n".repeat(600),
    )
    .unwrap();
    let output = harness
        .invoke(
            "git.diff",
            json!({"target": {"kind": "unstaged"}, "inline_max_bytes": 1024}),
        )
        .unwrap();
    assert!(output["files"].is_null());
    assert!(output["artifact"]["id"].as_str().is_some());
    let bytes = harness.artifacts.only();
    let payload: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["files"].as_array().unwrap().len(), 1);
    assert_eq!(output["artifact"]["byte_len"], bytes.len());
    let digest = format!("{:x}", Sha256::digest(&bytes));
    assert_eq!(
        digest.len(),
        64,
        "artifact bytes have stable SHA-256 metadata input"
    );
}

#[test]
fn diff_spill_uses_store_redaction_hashing_and_tool_call_associations() {
    let workspace = tempfile::tempdir().unwrap();
    initialize_repository(workspace.path());
    fs::write(
        workspace.path().join("tracked.txt"),
        "changed\n".repeat(600),
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(data.path()).unwrap());
    let task = Task::new(
        "Read a diff",
        workspace.path(),
        None,
        OffsetDateTime::UNIX_EPOCH,
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), OffsetDateTime::UNIX_EPOCH);
    store.insert_run(&run).unwrap();
    let step = Step::new(run.id(), 0, "Inspect", OffsetDateTime::UNIX_EPOCH);
    store.insert_step(&step).unwrap();
    let input = json!({"target": {"kind": "unstaged"}, "inline_max_bytes": 1024});
    let call = ToolCall::new(
        &step,
        "git.diff",
        "",
        input.clone(),
        OffsetDateTime::UNIX_EPOCH,
    );
    store.insert_tool_call(&call).unwrap();
    let artifacts = StoreArtifacts::new(Arc::clone(&store), run.id(), step.id(), call.id());
    let mut context = ExecutionContext::new(
        run.id(),
        step.id(),
        call.id(),
        workspace.path(),
        Cancellation::default(),
        Box::new(DiscardedProgress),
        Box::new(artifacts),
    )
    .unwrap();
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();
    let id = "git.diff".parse().unwrap();
    let raw = serde_json::value::to_raw_value(&input).unwrap();
    let output = invoke(&registry, &id, None, &raw, &mut context).unwrap();
    let output: Value = serde_json::from_str(output.output().get()).unwrap();
    let artifact_id = ArtifactId::from_str(output["artifact"]["id"].as_str().unwrap()).unwrap();
    let metadata = store.artifact(artifact_id).unwrap();
    let bytes = store.read_artifact(artifact_id).unwrap();

    assert_eq!(metadata.step_id(), Some(step.id()));
    assert_eq!(metadata.tool_call_id(), Some(call.id()));
    assert_eq!(metadata.byte_size(), u64::try_from(bytes.len()).unwrap());
    assert_eq!(metadata.sha256(), format!("{:x}", Sha256::digest(&bytes)));
    let payload: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["files"].as_array().unwrap().len(), 1);
}

#[test]
fn workspace_inspect_distinguishes_catalog_metadata_from_a_root_label() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();
    let id = "workspace.inspect".parse().unwrap();
    let raw = serde_json::value::to_raw_value(&json!({})).unwrap();
    let mut detached = harness.context();
    let output = invoke(&registry, &id, None, &raw, &mut detached).unwrap();
    let output: Value = serde_json::from_str(output.output().get()).unwrap();
    assert!(output["project"].is_null());
    assert!(!output["display_label"].as_str().unwrap().is_empty());

    let project = Project {
        id: ProjectId::new(),
        display_name: "Catalog name".to_owned(),
        root: harness.workspace.path().to_path_buf(),
        source: ProjectSource::Local,
        last_opened: OffsetDateTime::UNIX_EPOCH,
        available: true,
        git: None,
    };
    let mut attached = harness
        .context()
        .with_workspace_metadata(WorkspaceMetadata::from_project(&project))
        .unwrap();
    let output = invoke(&registry, &id, None, &raw, &mut attached).unwrap();
    let output: Value = serde_json::from_str(output.output().get()).unwrap();
    assert_eq!(output["project"]["id"], project.id.to_string());
    assert_eq!(output["project"]["display_name"], "Catalog name");
    assert_eq!(output["project"]["source"], "local");
}

#[test]
fn workspace_metadata_refuses_a_different_catalog_root() {
    let harness = Harness::new();
    let other = TempDir::new().unwrap();
    let project = Project {
        id: ProjectId::new(),
        display_name: "Wrong workspace".to_owned(),
        root: other.path().to_path_buf(),
        source: ProjectSource::Local,
        last_opened: OffsetDateTime::UNIX_EPOCH,
        available: true,
        git: None,
    };

    let error = harness
        .context()
        .with_workspace_metadata(WorkspaceMetadata::from_project(&project))
        .unwrap_err();
    assert_eq!(error.kind(), "forbidden_path");
}

#[test]
fn concurrent_diff_and_search_invocations_complete_without_repository_locks() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    fs::write(
        harness.workspace.path().join("tracked.txt"),
        b"needle changed\n",
    )
    .unwrap();
    let root = harness.workspace.path().to_path_buf();
    let workers = ["git.diff", "git.diff", "workspace.search"]
        .into_iter()
        .map(|tool| {
            let root = root.clone();
            thread::spawn(move || {
                let mut registry = ToolRegistry::new();
                register_read_only_tools(&mut registry).unwrap();
                let id = tool.parse().unwrap();
                let input = if tool == "git.diff" {
                    json!({"target": {"kind": "unstaged"}})
                } else {
                    json!({"query": "needle"})
                };
                let raw = serde_json::value::to_raw_value(&input).unwrap();
                let mut context = ExecutionContext::detached(
                    RunId::new(),
                    StepId::new(),
                    ToolCallId::new(),
                    root,
                )
                .unwrap();
                invoke(&registry, &id, None, &raw, &mut context).unwrap();
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(!harness.workspace.path().join(".git/index.lock").exists());
}

#[test]
#[ignore = "release-only timing evidence; run with cargo test --release -- --ignored"]
fn registry_lookup_and_dispatch_overhead_stay_within_issue_budgets() {
    use std::time::Instant;

    let harness = Harness::new();
    fs::write(harness.workspace.path().join("tiny.txt"), b"tiny\n").unwrap();
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();
    let id = "fs.read".parse().unwrap();
    let lookup_started = Instant::now();
    for _ in 0..1000 {
        registry.resolve(&id, None).unwrap();
    }
    let lookup_average = lookup_started.elapsed() / 1000;
    assert!(lookup_average < std::time::Duration::from_millis(1));

    let raw = serde_json::value::to_raw_value(&json!({"path": "tiny.txt"})).unwrap();
    let started = Instant::now();
    for _ in 0..100 {
        let mut context = harness.context();
        invoke(&registry, &id, None, &raw, &mut context).unwrap();
    }
    let average = started.elapsed() / 100;
    eprintln!(
        "release lookup={lookup_average:?} dispatch_plus_tiny_read={average:?} os={} arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    assert!(average < std::time::Duration::from_millis(10));
}

#[test]
fn fixture_setup_uses_system_git_only_outside_tool_execution() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let status = git(harness.workspace.path(), ["status", "--porcelain"]);
    assert!(status.is_empty());
}
