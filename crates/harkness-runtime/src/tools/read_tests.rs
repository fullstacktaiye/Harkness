use std::borrow::Cow;
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
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::OffsetDateTime;

use super::fs_read::MAX_FS_READ_RESULT_BYTES;
use super::register_read_only_tools;
use super::workspace_search::MAX_SEARCH_PATTERN_BYTES;
use crate::domain::{ArtifactId, Run, RunId, Step, StepId, Task, ToolCall, ToolCallId};
use crate::store::{Redactor, Store, StoreArtifacts};
use crate::tool::{
    ArtifactRef, ArtifactStream, ArtifactWriter, DiscardedProgress, ExecutionContext,
    InvocationError, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, ToolRegistry,
    WorkspaceMetadata, invoke,
};

#[derive(Debug)]
struct NonIdempotentMasking;

impl Redactor for NonIdempotentMasking {
    fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Owned(format!("R({})", text.replace("secret", "[masked]")))
    }

    fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
        sink
    }
}

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

    fn write_json(
        &mut self,
        name: &str,
        media_type: &str,
        value: &Value,
    ) -> Result<ArtifactRef, ToolError> {
        let bytes = serde_json::to_vec(value).map_err(ToolError::execution_failed)?;
        self.write(name, media_type, &bytes)
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

/// One file, one per-file omission — however many lines match.
///
/// The cap used a bare `break`, which left only the innermost match loop, so
/// every later matching line pushed the omission again. The records were never
/// charged to any budget, so a large file produced one per matching line and
/// grew without bound before tripping a guard that then discarded every match.
#[test]
fn a_file_past_the_per_file_cap_yields_exactly_one_omission_and_keeps_its_matches() {
    let harness = Harness::new();
    fs::write(
        harness.workspace.path().join("many.txt"),
        "needle\n".repeat(500),
    )
    .unwrap();

    let output = harness
        .invoke(
            "workspace.search",
            json!({"query": "needle", "max_per_file": 3}),
        )
        .unwrap();

    assert_eq!(
        output["matches"].as_array().unwrap().len(),
        3,
        "the retained matches are the cap, not zero"
    );
    let per_file = output["omissions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|omission| omission["kind"] == "per_file_match_budget_exhausted")
        .count();
    assert_eq!(per_file, 1, "one file may name its cap once");
}

/// Omissions are charged to the output budget, and matches are never discarded
/// to pay for them.
///
/// Only matches used to be measured, so a workspace with enough unsearchable
/// files overran the limit with omissions alone; the terminal guard answered by
/// clearing everything and reporting "output budget exhausted" — a query that
/// matched came back as nothing found.
#[test]
fn omissions_are_budgeted_and_never_cost_a_match_that_fit() {
    let harness = Harness::new();
    for index in 0..400 {
        fs::write(
            harness
                .workspace
                .path()
                .join(format!("blob-{index:04}.bin")),
            [0xff, 0x00],
        )
        .unwrap();
    }
    fs::write(harness.workspace.path().join("prose.txt"), "needle here\n").unwrap();

    let output = harness
        .invoke("workspace.search", json!({"query": "needle"}))
        .unwrap();

    assert_eq!(
        output["matches"].as_array().unwrap().len(),
        1,
        "the one real match must survive a flood of omissions"
    );
    assert_eq!(output["matches"][0]["path"], "prose.txt");
    assert!(
        output["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|omission| omission["kind"] == "output_budget_exhausted"),
        "the truncated omission list is still named"
    );
}

/// Containment decides before anything opens a path.
///
/// The symlink pre-check used to run on the caller's raw string, so an absolute
/// path anywhere on the host was walked component by component first. Its
/// distinct refusals answered "does this exist, is it a link, is it a
/// directory, is it readable" for paths the caller was never allowed to name.
#[test]
fn an_absolute_path_outside_the_workspace_is_refused_without_being_probed() {
    let harness = Harness::new();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(outside.path().join("real-directory")).unwrap();

    let mut kinds = Vec::new();
    for candidate in [
        outside.path().join("real-directory"),
        outside.path().join("does-not-exist"),
    ] {
        let error = harness
            .invoke(
                "workspace.search",
                json!({"query": "needle", "path": candidate.to_str().unwrap()}),
            )
            .unwrap_err();
        kinds.push(error.kind().to_owned());
    }

    assert_eq!(
        kinds[0], kinds[1],
        "an existing and a missing outside path must be indistinguishable, \
         or the refusal is a filesystem oracle: {kinds:?}"
    );
    assert_eq!(kinds[0], "outside_allowed_roots");
}

/// A `..` segment does not route around the symlink refusal.
///
/// The lexical walk filtered to `Component::Normal`, silently dropping `..`, so
/// it inspected a different path than the one that was resolved.
#[cfg(unix)]
#[test]
fn a_parent_segment_does_not_smuggle_a_search_root_through_a_symlink() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    let real = harness.workspace.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("hit.txt"), "needle\n").unwrap();
    fs::create_dir(harness.workspace.path().join("plain")).unwrap();
    symlink(&real, harness.workspace.path().join("link")).unwrap();

    let direct = harness
        .invoke(
            "workspace.search",
            json!({"query": "needle", "path": "link"}),
        )
        .unwrap_err();
    assert_eq!(direct.kind(), "forbidden_path");

    let smuggled = harness
        .invoke(
            "workspace.search",
            json!({"query": "needle", "path": "plain/../link"}),
        )
        .unwrap_err();
    assert_eq!(
        smuggled.kind(),
        "forbidden_path",
        "a `..` segment must not reach a root the direct spelling refuses"
    );
}

/// The pattern bound is measured in bytes, as its constant is named.
///
/// The published schema's `length` keyword counts code points, so a pattern of
/// multi-byte characters passed it at four times the declared byte limit.
#[test]
fn a_pattern_over_the_byte_bound_is_refused_even_when_its_character_count_fits() {
    let harness = Harness::new();
    let query = "\u{1F600}".repeat(MAX_SEARCH_PATTERN_BYTES / 4 + 1);
    assert!(
        query.chars().count() <= 1024,
        "the schema's length must fit"
    );

    let error = harness
        .invoke("workspace.search", json!({"query": query}))
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_input");
}

/// A read stays under the store's inline bound whatever the bytes escape to.
///
/// `max_bytes` counts decoded bytes; JSON escaping turns each C0 control byte
/// into six characters, so a 32 KiB window serialized to ~196 KB and the store
/// refused it — recording a `payload_too_large` failure for a read made
/// entirely within the published schema.
#[test]
fn a_control_dense_read_fits_the_inline_bound_and_names_its_truncation() {
    let harness = Harness::new();
    fs::write(
        harness.workspace.path().join("escapes.txt"),
        vec![0x01_u8; 32 * 1024],
    )
    .unwrap();

    let output = harness
        .invoke("fs.read", json!({"path": "escapes.txt"}))
        .unwrap();

    let encoded = serde_json::to_vec(&output).unwrap();
    assert!(
        encoded.len() <= MAX_FS_READ_RESULT_BYTES,
        "serialized result was {} bytes, above the {MAX_FS_READ_RESULT_BYTES} byte inline bound",
        encoded.len()
    );
    assert_eq!(output["truncated"]["kind"], "byte_limit");
    assert_eq!(output["byte_size"], 32 * 1024);
    assert!(output["returned_bytes"].as_u64().unwrap() > 0);
}

/// Every tool's published schema is closed, and every tool refuses a stray key.
///
/// `schemars` closes an object schema only for a type carrying
/// `#[serde(deny_unknown_fields)]`, so this is what stops an agent's misspelled
/// field being discarded in silence. It was proven for `fs.read` alone.
#[test]
fn every_read_tool_publishes_a_closed_schema_and_refuses_an_unknown_field() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();

    for (id, mut input) in [
        ("workspace.inspect", json!({})),
        ("fs.read", json!({"path": "tracked.txt"})),
        ("workspace.search", json!({"query": "needle"})),
        ("git.status", json!({})),
        ("git.diff", json!({"target": {"kind": "unstaged"}})),
    ] {
        let descriptor = registry
            .resolve(&id.parse().unwrap(), None)
            .unwrap()
            .descriptor();
        assert_eq!(descriptor.risk(), RiskLevel::Observe, "{id}");
        assert!(!descriptor.title().is_empty(), "{id}");
        assert!(!descriptor.description().is_empty(), "{id}");
        assert!(!descriptor.spawns_processes(), "{id}");
        assert!(descriptor.output_schema().is_object(), "{id}");
        assert_eq!(
            descriptor.input_schema().get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{id} publishes an open input schema, so a misspelled field is dropped silently"
        );

        input
            .as_object_mut()
            .unwrap()
            .insert("no_such_field".to_owned(), json!(1));
        let error = harness.invoke(id, input).unwrap_err();
        assert_eq!(error.kind(), "invalid_input", "{id}");
    }
}

/// `fs.read`'s line addressing, its line-limit truncation, and its mode bit.
///
/// `offset`, `limit`, `ReadTruncation::LineLimit`, `returned_bytes` and
/// `executable` had no assertions at all, and `read_lines`' `consumed <
/// byte_size` guard — which decides whether a range ending exactly at EOF is
/// reported as truncated — was unexercised in both directions.
#[test]
fn fs_read_addresses_lines_and_reports_the_mode_bit() {
    let harness = Harness::new();
    let path = harness.workspace.path().join("lines.txt");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

    let middle = harness
        .invoke(
            "fs.read",
            json!({"path": "lines.txt", "offset": 1, "limit": 2}),
        )
        .unwrap();
    assert_eq!(middle["content"], "two\nthree\n");
    assert_eq!(middle["offset"], 1);
    assert_eq!(middle["truncated"]["kind"], "line_limit");
    assert_eq!(middle["truncated"]["limit"], 2);
    assert_eq!(middle["content_encoding"], "utf8");
    assert_eq!(middle["media_type"], "text/plain");
    assert_eq!(middle["returned_bytes"], 10);
    assert_eq!(middle["byte_size"], 24);

    // A range ending exactly at end of file is complete, not truncated.
    let tail = harness
        .invoke(
            "fs.read",
            json!({"path": "lines.txt", "offset": 3, "limit": 2}),
        )
        .unwrap();
    assert_eq!(tail["content"], "four\nfive\n");
    assert_eq!(tail["truncated"], Value::Null);

    assert_eq!(
        harness
            .invoke("fs.read", json!({"path": "lines.txt"}))
            .unwrap()["executable"],
        false
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        assert_eq!(
            harness
                .invoke("fs.read", json!({"path": "lines.txt"}))
                .unwrap()["executable"],
            true
        );
    }
}

/// `workspace.inspect` reports the Git summary its scope names.
///
/// `head`, `dirty`, `staged`, `unstaged` and `upstream` had no assertions, so
/// the whole `WorkspaceGitSummary` projection — including the `head_state()`
/// call and its failure branch — was unexercised.
#[test]
fn workspace_inspect_reports_head_and_dirty_state_or_null_outside_a_repository() {
    let harness = Harness::new();
    assert_eq!(
        harness.invoke("workspace.inspect", json!({})).unwrap()["git"],
        Value::Null,
        "a plain directory is not a repository"
    );

    initialize_repository(harness.workspace.path());
    fs::write(
        harness.workspace.path().join("tracked.txt"),
        b"unstaged change\n",
    )
    .unwrap();
    fs::write(harness.workspace.path().join("added.txt"), b"staged\n").unwrap();
    git(harness.workspace.path(), ["add", "added.txt"]);

    let git_summary = &harness.invoke("workspace.inspect", json!({})).unwrap()["git"];
    assert_eq!(git_summary["head"]["kind"], "branch");
    assert!(
        git_summary["head"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "a born branch is named: {git_summary}"
    );
    assert_eq!(git_summary["dirty"], true);
    assert_eq!(git_summary["staged"], 1);
    assert_eq!(git_summary["unstaged"], 1);
    assert_eq!(git_summary["upstream"], Value::Null);
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

#[cfg(unix)]
#[test]
fn search_refuses_internal_directory_symlinks_and_git_administration() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    fs::create_dir(harness.workspace.path().join("real")).unwrap();
    fs::create_dir(harness.workspace.path().join("real/subdir")).unwrap();
    fs::write(
        harness.workspace.path().join("real/subdir/file.txt"),
        "needle\n",
    )
    .unwrap();
    symlink("real", harness.workspace.path().join("internal_link")).unwrap();

    assert_eq!(
        harness
            .invoke(
                "workspace.search",
                json!({"path": "internal_link/subdir", "query": "needle"}),
            )
            .unwrap_err()
            .kind(),
        "forbidden_path"
    );
    assert_eq!(
        harness
            .invoke(
                "workspace.search",
                json!({"path": ".git/objects", "query": "needle"}),
            )
            .unwrap_err()
            .kind(),
        "forbidden_path"
    );
}

#[test]
fn invalid_regex_is_rejected_before_search_observes_its_path() {
    let harness = Harness::new();
    let error = harness
        .invoke(
            "workspace.search",
            json!({"path": "missing", "query": "(", "regex": true}),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "invalid_input");
}

#[test]
fn a_regex_match_longer_than_the_excerpt_is_still_clamped() {
    let harness = Harness::new();
    fs::write(harness.workspace.path().join("long.txt"), "x".repeat(200)).unwrap();
    let output = harness
        .invoke(
            "workspace.search",
            json!({
                "query": "x{100}",
                "regex": true,
                "max_excerpt_bytes": 16
            }),
        )
        .unwrap();
    assert_eq!(output["matches"][0]["excerpt"].as_str().unwrap().len(), 16);
}

/// A FIFO and a binary file are named, and the FIFO is never opened.
///
/// Portable names on purpose: `#[cfg(unix)]` says the platform has FIFOs, not
/// that its filesystem accepts arbitrary bytes in a name. APFS and HFS+ enforce
/// valid UTF-8 and refuse one with `EILSEQ`, which is why the lossy-path half
/// of this claim lives in its own Linux-gated test below.
#[cfg(unix)]
#[test]
fn search_names_nonregular_and_binary_omissions_without_opening_them() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    let harness = Harness::new();
    let fifo = harness.workspace.path().join("waiting-fifo");
    let encoded = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) },
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );
    fs::write(harness.workspace.path().join("binary.bin"), [0xff, 0]).unwrap();

    // No writer is ever attached. Opening this FIFO would block forever, so
    // returning at all is the assertion: the kind is decided from the entry's
    // metadata rather than by opening it.
    let started = Instant::now();
    let output = harness
        .invoke("workspace.search", json!({"query": "needle"}))
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));

    let omissions = output["omissions"].as_array().unwrap();
    let non_regular = omissions
        .iter()
        .find(|omission| omission["kind"] == "non_regular_file")
        .expect("the FIFO is named");
    assert_eq!(non_regular["path"], "waiting-fifo");
    assert_eq!(non_regular["path_is_lossy"], false);
    let binary = omissions
        .iter()
        .find(|omission| omission["kind"] == "binary_file")
        .expect("the binary file is named");
    assert_eq!(binary["path"], "binary.bin");
}

/// A name no lossy conversion can round-trip carries its exact bytes.
///
/// Linux only: Darwin filesystems reject an invalid UTF-8 filename with
/// `EILSEQ` before Harkness can observe it, the same reason
/// `harkness-git`'s commit and diff suites gate their equivalents.
#[cfg(target_os = "linux")]
#[test]
fn search_reports_non_utf8_omission_paths_as_lossy_with_exact_bytes() {
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let harness = Harness::new();
    let fifo_name = OsString::from_vec(b"fifo-\xff".to_vec());
    let fifo = harness.workspace.path().join(&fifo_name);
    let encoded = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) },
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );
    let binary_name = OsString::from_vec(b"binary-\xfe".to_vec());
    fs::write(harness.workspace.path().join(&binary_name), [0xff, 0]).unwrap();

    let output = harness
        .invoke("workspace.search", json!({"query": "needle"}))
        .unwrap();
    for (kind, expected) in [
        ("non_regular_file", b"fifo-\xff".to_vec()),
        ("binary_file", b"binary-\xfe".to_vec()),
    ] {
        let omission = output["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|omission| omission["kind"] == kind)
            .unwrap_or_else(|| panic!("{kind} is named"));
        assert_eq!(omission["path_is_lossy"], true);
        let decoded = BASE64
            .decode(omission["path_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, expected, "{kind} must carry its exact bytes");
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
fn git_status_honors_disabled_rename_detection_and_entry_bounds() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    git(
        harness.workspace.path(),
        ["config", "status.renames", "false"],
    );
    git(
        harness.workspace.path(),
        ["mv", "tracked.txt", "renamed.txt"],
    );

    let full = harness.invoke("git.status", json!({})).unwrap();
    assert!(
        full["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["path"] == "tracked.txt" && entry["staged"] == "deleted" })
    );
    assert!(
        full["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["path"] == "renamed.txt" && entry["staged"] == "added" })
    );

    let bounded = harness
        .invoke("git.status", json!({"max_entries": 1}))
        .unwrap();
    assert_eq!(bounded["entries"].as_array().unwrap().len(), 1);
    assert_eq!(bounded["omission"]["kind"], "entry_budget_exhausted");
    assert_eq!(bounded["omission"]["omitted_entries"], 1);
}

/// A staged rename must be reported under its *destination*.
///
/// libgit2's status entry path is the delta's old file, so taking it verbatim
/// named the source twice and dropped the new name entirely. This is the
/// default configuration — the sibling test disables rename detection, which
/// routes around the whole question.
#[test]
fn git_status_reports_a_staged_rename_under_its_destination() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    git(
        harness.workspace.path(),
        ["mv", "tracked.txt", "renamed.txt"],
    );

    let output = harness.invoke("git.status", json!({})).unwrap();
    let entries = output["entries"].as_array().unwrap();
    let renamed = entries
        .iter()
        .find(|entry| entry["staged"] == "renamed")
        .expect("the staged rename is reported");
    assert_eq!(renamed["path"], "renamed.txt");
    assert_eq!(renamed["rename_source"], "tracked.txt");
    assert!(
        !entries.iter().any(|entry| entry["path"] == "tracked.txt"),
        "the source name must not also be reported as a present path"
    );
}

/// `status.renames=copies` means Git's `-C`, not `--find-copies-harder`.
///
/// Git reports a copy of an *unmodified* file as a plain add, so the source has
/// to change in the same index for a copy to be classified as one. Asserting
/// the aggressive spelling instead pinned a divergence from the CLI projection.
#[test]
fn git_status_honors_copy_detection_configuration() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    git(
        harness.workspace.path(),
        ["config", "status.renames", "copies"],
    );
    let source = harness.workspace.path().join("tracked.txt");
    fs::copy(&source, harness.workspace.path().join("copied.txt")).unwrap();
    let mut extended = fs::read(&source).unwrap();
    extended.extend_from_slice(b"an added line\n");
    fs::write(&source, extended).unwrap();
    git(harness.workspace.path(), ["add", "-A"]);

    let output = harness.invoke("git.status", json!({})).unwrap();
    let copy = output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "copied.txt")
        .unwrap();
    assert_eq!(copy["staged"], "copied");
    assert_eq!(copy["rename_source"], "tracked.txt");
}

/// A copy of an unmodified file is an `added` path, exactly as Git says.
#[test]
fn git_status_does_not_invent_a_copy_git_itself_reports_as_added() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    git(
        harness.workspace.path(),
        ["config", "status.renames", "copies"],
    );
    fs::copy(
        harness.workspace.path().join("tracked.txt"),
        harness.workspace.path().join("copied.txt"),
    )
    .unwrap();
    git(harness.workspace.path(), ["add", "copied.txt"]);

    let output = harness.invoke("git.status", json!({})).unwrap();
    let copy = output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "copied.txt")
        .unwrap();
    assert_eq!(copy["staged"], "added");
    assert_eq!(copy["rename_source"], serde_json::Value::Null);
}

/// Git accepts `copy` as well as `copies`, and a value neither Git nor this
/// build recognizes must not turn a read into a refusal.
#[test]
fn git_status_accepts_every_rename_spelling_git_accepts() {
    for (value, expect_copy) in [("copy", true), ("copies", true), ("2", false)] {
        let harness = Harness::new();
        initialize_repository(harness.workspace.path());
        git(
            harness.workspace.path(),
            ["config", "status.renames", value],
        );
        let source = harness.workspace.path().join("tracked.txt");
        fs::copy(&source, harness.workspace.path().join("copied.txt")).unwrap();
        let mut extended = fs::read(&source).unwrap();
        extended.extend_from_slice(b"an added line\n");
        fs::write(&source, extended).unwrap();
        git(harness.workspace.path(), ["add", "-A"]);

        let output = harness
            .invoke("git.status", json!({}))
            .unwrap_or_else(|error| panic!("status.renames={value} was refused: {error:?}"));
        let copy = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == "copied.txt")
            .unwrap();
        assert_eq!(
            copy["staged"] == "copied",
            expect_copy,
            "status.renames={value} classified {copy}"
        );
    }
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
    // The digest of the *stored bytes* against the digest of the payload they
    // decode to. Asserting the digest's length proved nothing: every SHA-256
    // renders as 64 hex characters, for every input, including none at all.
    // The store-side comparison lives in
    // `diff_spill_uses_store_redaction_hashing_and_tool_call_associations`.
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&payload).unwrap())
        ),
        "the artifact bytes are exactly the serialized payload"
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
fn diff_redacts_content_once_without_rewriting_inline_controls_or_artifact_refs() {
    let workspace = tempfile::tempdir().unwrap();
    initialize_repository(workspace.path());
    fs::write(workspace.path().join("tracked.txt"), "secret changed\n").unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(data.path())
            .unwrap()
            .redacting(Arc::new(NonIdempotentMasking)),
    );
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
    let call = ToolCall::new(
        &step,
        "git.diff",
        "",
        json!({"target": {"kind": "unstaged"}}),
        OffsetDateTime::UNIX_EPOCH,
    );
    store.insert_tool_call(&call).unwrap();
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();
    let id = "git.diff".parse().unwrap();

    let invoke_with = |input: Value| {
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
        let raw = serde_json::value::to_raw_value(&input).unwrap();
        let output = invoke(&registry, &id, None, &raw, &mut context).unwrap();
        serde_json::from_str::<Value>(output.output().get()).unwrap()
    };

    let inline = invoke_with(json!({"target": {"kind": "unstaged"}}));
    let file = &inline["files"][0];
    assert_eq!(file["target"], "unstaged");
    assert_eq!(file["hunks"][0]["lines"][0]["content_encoding"], "utf8");
    assert!(inline.to_string().contains("R([masked] changed"));
    assert!(!inline.to_string().contains("R(R("));

    fs::write(
        workspace.path().join("tracked.txt"),
        "secret changed\n".repeat(600),
    )
    .unwrap();
    let spilled = invoke_with(json!({
        "target": {"kind": "unstaged"},
        "inline_max_bytes": 1024
    }));
    let artifact_id = ArtifactId::from_str(spilled["artifact"]["id"].as_str().unwrap()).unwrap();
    let metadata = store.artifact(artifact_id).unwrap();
    assert_eq!(
        spilled["artifact"]["media_type"].as_str().unwrap(),
        metadata.media_type()
    );
    let payload: Value =
        serde_json::from_slice(&store.read_artifact(artifact_id).unwrap()).unwrap();
    assert!(payload.get("files").is_some(), "published keys stay intact");
    assert!(payload.to_string().contains("R([masked] changed"));
    assert!(!payload.to_string().contains("R(R("));
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
        checks: None,
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
        checks: None,
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
fn workspace_inspect_stops_at_the_requested_entry_sentinel() {
    let harness = Harness::new();
    for name in ["one", "two", "three", "four"] {
        fs::write(harness.workspace.path().join(name), b"").unwrap();
    }

    let output = harness
        .invoke("workspace.inspect", json!({"max_entries": 2}))
        .unwrap();

    assert_eq!(output["entries"].as_array().unwrap().len(), 2);
    assert_eq!(output["omission"]["kind"], "entry_budget_exhausted");
    assert_eq!(output["omission"]["at_least_omitted_entries"], 1);
}

#[test]
fn search_stops_traversal_at_its_entry_budget() {
    let harness = Harness::new();
    for index in 0..=super::workspace_search::MAX_SEARCH_ENTRIES {
        fs::write(
            harness.workspace.path().join(format!("file-{index:05}")),
            b"no match\n",
        )
        .unwrap();
    }

    let output = harness
        .invoke("workspace.search", json!({"query": "needle"}))
        .unwrap();

    assert!(output["omissions"].as_array().unwrap().iter().any(|item| {
        item["kind"] == "file_budget_exhausted"
            && item["limit"] == super::workspace_search::MAX_SEARCH_FILES
    }));
    assert!(
        output["scanned_files"].as_u64().unwrap()
            <= u64::try_from(super::workspace_search::MAX_SEARCH_FILES).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn git_diff_preserves_a_literal_symlink_path_filter() {
    use std::os::unix::fs::symlink;

    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    symlink("tracked.txt", harness.workspace.path().join("tracked-link")).unwrap();
    git(harness.workspace.path(), ["add", "tracked-link"]);
    git(
        harness.workspace.path(),
        ["commit", "-m", "track literal symlink"],
    );
    fs::remove_file(harness.workspace.path().join("tracked-link")).unwrap();
    symlink(
        "other-target",
        harness.workspace.path().join("tracked-link"),
    )
    .unwrap();

    let output = harness
        .invoke(
            "git.diff",
            json!({
                "target": {"kind": "unstaged"},
                "paths": ["tracked-link"]
            }),
        )
        .unwrap();

    assert_eq!(output["files"].as_array().unwrap().len(), 1);
    assert_eq!(output["files"][0]["new_path"], "tracked-link");
    assert_eq!(output["files"][0]["provenance"], Value::Null);
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
    // `.git/index.lock` is Git's own transient lock and is not what the
    // acceptance criterion is about: `RepositoryLock` lives under `locks/` in
    // the data directory a `GitService` was built with, never inside `.git`.
    // Asserting the wrong path passed with no tool invoked at all. These tools
    // build their service with the workspace root as that data directory, so
    // `locks/` here is exactly where a lock would land if a read ever took one.
    assert!(
        !harness.workspace.path().join("locks").exists(),
        "a read-only tool acquired the repository lock"
    );
}

/// A tool whose body does nothing, so a dispatch measurement is only dispatch.
#[derive(Clone, Copy, Debug, Default)]
struct NoOpTool;

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoOpInput {}

#[derive(Clone, Copy, Debug, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct NoOpOutput {}

impl Tool for NoOpTool {
    type Input = NoOpInput;
    type Output = NoOpOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("bench.noop", "1.0.0").expect("a benchmark tool identity"),
            "No-op",
            "Returns immediately so a dispatch measurement excludes tool work.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: Self::Input,
        _context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        Ok(NoOpOutput {})
    }
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn registry_lookup_and_dispatch_overhead_stay_within_issue_budgets() {
    use std::time::{Duration, Instant};

    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "the dispatch overhead benchmark must run with --release"
    );

    let harness = Harness::new();
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).unwrap();
    registry.register(NoOpTool).unwrap();
    let id = "fs.read".parse().unwrap();
    let lookup_started = Instant::now();
    for _ in 0..1000 {
        registry.resolve(&id, None).unwrap();
    }
    let lookup_average = lookup_started.elapsed() / 1000;

    // Against a no-op body, so what is measured is the pipeline the acceptance
    // criterion names — resolve, validate input, deserialize, `catch_unwind`,
    // serialize, validate output — and not a real file read. Timing `fs.read`
    // could not tell a dispatch regression from a slow filesystem.
    let raw = serde_json::value::to_raw_value(&json!({})).unwrap();
    let noop = "bench.noop".parse().unwrap();
    let started = Instant::now();
    for _ in 0..1000 {
        let mut context = harness.context();
        invoke(&registry, &noop, None, &raw, &mut context).unwrap();
    }
    let dispatch_average = started.elapsed() / 1000;

    // Printed rather than eprintln!'d so `--nocapture` in CI records it.
    println!(
        "lookup={lookup_average:?} dispatch={dispatch_average:?} os={} arch={} parallelism={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(0),
    );
    assert!(
        lookup_average < Duration::from_millis(1),
        "registry lookup took {lookup_average:?}"
    );
    assert!(
        dispatch_average < Duration::from_millis(10),
        "dispatch overhead took {dispatch_average:?}"
    );
}

/// The fixture helper shells out to Git; the tools themselves never do.
///
/// Named for what it checks. Its previous name promised a property of tool
/// execution that its body — which invokes no tool — could not observe.
#[test]
fn the_repository_fixture_helper_uses_system_git() {
    let harness = Harness::new();
    initialize_repository(harness.workspace.path());
    let status = git(harness.workspace.path(), ["status", "--porcelain"]);
    assert!(status.is_empty());
}
