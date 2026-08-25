//! `example.word_count` — one complete tool, from declaration to invocation.
//!
//! This file is the worked example in `docs/tool-authoring.md`. The two are
//! compared byte for byte by
//! `the_tool_authoring_example_is_the_file_it_claims_to_be`, so
//! the documented code is code that compiles, and running it proves it works:
//!
//! ```sh
//! cargo run -p harkness-runtime --example word_count_tool
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use harkness_git::Cancellation;
use harkness_runtime::domain::{RunId, StepId, ToolCallId};
use harkness_runtime::tool::{
    ArtifactRef, ArtifactStream, ArtifactWriter, ExecutionContext, ProgressEvent, ProgressUnit,
    RecordedProgress, RequestEffects, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata,
    ToolRegistry, invoke,
};
use harkness_runtime::trust::{PathAccess, PathBoundary};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// What the tool accepts. Its JSON Schema is generated from this type.
///
/// `deny_unknown_fields` is what closes the published schema. Without it an
/// agent's misspelled field is discarded by serde instead of reported, so every
/// `Input` type carries it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WordCountInput {
    /// Workspace-relative path to count. Resolved through the call's boundary.
    path: String,
    /// Whether blank lines are left out of the counts.
    #[serde(default)]
    skip_blank_lines: bool,
}

/// What the tool returns. Its JSON Schema is generated from this type too.
#[derive(Serialize, JsonSchema)]
struct WordCountOutput {
    /// The path as it was requested, echoed so a result is self-describing.
    path: String,
    /// Lines counted, after `skip_blank_lines` was applied.
    lines: u64,
    /// Whitespace-separated words across those lines.
    words: u64,
    /// Size of the file that was read.
    bytes: u64,
    /// Per-line counts, stored outside the result rather than inside it.
    report: ArtifactRef,
}

struct WordCount;

impl Tool for WordCount {
    type Input = WordCountInput;
    type Output = WordCountOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("example.word_count", "1.0.0").expect("a valid identity"),
            "Count words in a workspace file",
            "Reads one contained file and reports its line, word, and byte counts.",
            // Declared once and never lowered for a particular call. A call that
            // turns out to be more consequential is caught when it is evaluated.
            RiskLevel::Observe,
        )
    }

    /// Derives the policy facts of one *validated* input without executing it.
    ///
    /// The path is resolved here so policy sees a contained capability rather
    /// than a string, and so a path that escapes the workspace is refused before
    /// anything is recorded as pending.
    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        let path = boundary.contain(&input.path)?;
        Ok(RequestEffects::default().with_path(path, PathAccess::Read))
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        // Resolved again at execution: `request_effects` ran against the input,
        // and this is the capability the body is allowed to open.
        let path = context.resolve(&input.path)?;
        context.report(ProgressEvent::stage("reading"));

        let text = std::fs::read_to_string(path.as_path()).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolError::NotFound {
                    path: PathBuf::from(&input.path),
                }
            } else {
                ToolError::execution_failed(format!("{} could not be read: {error}", input.path))
            }
        })?;

        let mut lines = 0;
        let mut words = 0;
        let mut report = String::new();
        for (index, line) in text.lines().enumerate() {
            // The executor enforces the deadline and the token from outside, but
            // a body that notices first stops sooner and unwinds its own work.
            context.check_still_permitted()?;
            if input.skip_blank_lines && line.trim().is_empty() {
                continue;
            }
            let counted = count(line.split_whitespace().count());
            lines += 1;
            words += counted;
            report.push_str(&format!("{}\t{counted}\n", index + 1));
            context.report(ProgressEvent::indeterminate(lines, ProgressUnit::Items));
        }

        // Anything that can grow with the input belongs in an artifact. The
        // result carries a reference; the bytes never travel through a column.
        let stored = context.write_artifact(
            "word-count.tsv",
            "text/tab-separated-values",
            report.as_bytes(),
        )?;

        Ok(WordCountOutput {
            path: input.path,
            lines,
            words,
            bytes: count(text.len()),
            report: stored,
        })
    }
}

/// Widens a count for the wire, saturating rather than wrapping.
///
/// A published schema says `integer`, so the field is a `u64` whatever the
/// platform's `usize` is.
fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A workspace with one file in it. A real call takes its root from the run.
    let workspace = tempfile::tempdir()?;
    std::fs::write(
        workspace.path().join("poem.txt"),
        "the wide sea\n\nand sky\n",
    )?;

    // Registration is where schemas are generated and compiled into validators,
    // so a tool that cannot publish a valid contract fails here rather than on
    // its first call.
    let mut registry = ToolRegistry::new();
    registry.register(WordCount)?;

    let progress = RecordedProgress::new();
    let artifacts = InMemoryArtifacts::default();
    let stored = Arc::clone(&artifacts.stored);
    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        std::fs::canonicalize(workspace.path())?,
        Cancellation::default(),
        Box::new(progress.clone()),
        Box::new(artifacts),
    )?;

    let input = RawValue::from_string(r#"{"path":"poem.txt"}"#.to_owned())?;
    let outcome = invoke(
        &registry,
        &"example.word_count".parse()?,
        None,
        &input,
        &mut context,
    )?;

    // The version that actually ran travels with the result, because that pair
    // is what a recorded call stores and what an approval is matched against.
    assert_eq!(outcome.tool().to_string(), "example.word_count@1.0.0");

    let result: serde_json::Value = serde_json::from_str(outcome.output().get())?;
    assert_eq!(result["lines"], 3);
    assert_eq!(result["words"], 5);
    assert_eq!(result["bytes"], 22);
    assert_eq!(result["report"]["media_type"], "text/tab-separated-values");
    assert_eq!(
        stored.lock().expect("no panic while stored")[0].0,
        "word-count.tsv"
    );
    assert!(!progress.events().is_empty());

    // Object keys come back sorted, whatever order the `Output` declares them
    // in, so a hash taken over a recorded result is stable across builds.
    println!("{}", outcome.output().get());

    // The published contract is generated from the two types above.
    let descriptor = registry
        .resolve(&"example.word_count".parse()?, None)?
        .descriptor();
    println!(
        "{}",
        serde_json::to_string_pretty(descriptor.input_schema())?
    );

    Ok(())
}

/// A minimal artifact store, so the example runs outside a coordinator.
///
/// The real one is the run store's, which redacts every byte, hashes what it
/// wrote, and puts the content under `<data_dir>/artifacts/`. A tool never sees
/// the difference: it writes through this seam and puts the returned reference
/// in its result.
#[derive(Default)]
struct InMemoryArtifacts {
    stored: StoredArtifacts,
}

/// Everything this example's store has been handed, by name.
type StoredArtifacts = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

impl ArtifactWriter for InMemoryArtifacts {
    fn open(&mut self, name: &str, media_type: &str) -> Result<Box<dyn ArtifactStream>, ToolError> {
        Ok(Box::new(InMemoryArtifact {
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            bytes: Vec::new(),
            stored: Arc::clone(&self.stored),
        }))
    }
}

struct InMemoryArtifact {
    name: String,
    media_type: String,
    bytes: Vec<u8>,
    stored: StoredArtifacts,
}

impl Write for InMemoryArtifact {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ArtifactStream for InMemoryArtifact {
    /// Bytes are durable only once this returns; an abandoned stream records
    /// nothing, which is why the reference is built here and not at `open`.
    fn finish(self: Box<Self>) -> Result<ArtifactRef, ToolError> {
        let this = *self;
        let reference = ArtifactRef {
            id: this.name.clone(),
            media_type: this.media_type,
            byte_len: count(this.bytes.len()),
        };
        this.stored
            .lock()
            .map_err(|_| ToolError::execution_failed("the artifact store was poisoned"))?
            .push((this.name, this.bytes));
        Ok(reference)
    }
}
