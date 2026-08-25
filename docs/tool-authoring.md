# Writing a tool

A tool is the only way work happens in Harkness. Every front end and every agent
performs the same underlying operations, and the tempting shortcut is to let each
of them pass a string to something that interprets it. That shortcut is what
makes a policy unenforceable: a decision about "running a shell command" cannot
be made about a blob nobody has parsed yet, and an approval cannot be bound to
work whose shape is only known once it has started.

So a tool declares its input and output as Rust types, the schemas are
*generated* from those types, and the registry refuses to publish anything it
cannot validate. What the window runs, what the command line runs, and what an
agent runs is then the same typed operation under the same gates.

- [The shape of a tool](#the-shape-of-a-tool)
- [A complete example](#a-complete-example)
- [Identity and versions](#identity-and-versions)
- [Schemas are generated, never declared](#schemas-are-generated-never-declared)
- [Risk and capabilities](#risk-and-capabilities)
- [Declaring the effects of one call](#declaring-the-effects-of-one-call)
- [The execution context](#the-execution-context)
- [Timeouts and cancellation](#timeouts-and-cancellation)
- [Progress](#progress)
- [Artifacts](#artifacts)
- [Errors](#errors)
- [Child processes](#child-processes)
- [Registering it](#registering-it)
- [The checklist](#the-checklist)
- [What proves this](#what-proves-this)

## The shape of a tool

`Tool` has three methods and two associated types, and one of the three has a
default:

```rust
pub trait Tool: Send + Sync {
    type Input: DeserializeOwned + JsonSchema;
    type Output: Serialize + JsonSchema;

    fn metadata(&self) -> ToolMetadata;

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> { /* adds nothing by default */ }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError>;
}
```

Notice what is *not* there. There is no method returning a schema, no method
returning a risk level per call, and no method that decides whether the call may
proceed. Everything published about a tool is derived from the two types and the
one metadata value, so there is no way to implement this trait and end up with a
published contract that disagrees with what the body actually handles.

Around it, `invoke` runs six steps in a fixed order:

```text
1. Validate the input   against the published JSON Schema
2. Deserialize          into Self::Input
3. Execute              the body, inside a panic boundary
4. Serialize            Self::Output, with every object key sorted
5. Validate the output  against the published JSON Schema
6. Return               the result together with the (id, version) that ran
```

The order carries the guarantees. **Validation precedes execution**, so a
rejected input means the body provably never ran and a caller may retry a
correction without wondering about side effects. **Validation precedes policy**,
because policy must classify what will actually execute rather than an unparsed
blob. And **the output is validated before delivery**, so a consumer that trusted
the published schema never receives a shape it cannot handle; a tool that emits
the wrong thing produces a structured `InvalidOutput` rather than a downstream
crash.

Step 4 sorts every object key by its exact bytes, so a hash taken over a recorded
result is stable across builds and two tools declaring the same fields in
different orders produce byte-identical output.

## A complete example

The code below is `crates/harkness-runtime/examples/word_count_tool.rs`. It is
not a sketch: it compiles with the rest of the workspace, and
`the_tool_authoring_example_is_the_file_it_claims_to_be` compares
this fenced block against that file byte for byte, so the two cannot drift.

```sh
cargo run -p harkness-runtime --example word_count_tool
```

<!-- mirrors: crates/harkness-runtime/examples/word_count_tool.rs -->
```rust
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
```

Running it prints the validated result and the schema that was generated from
`WordCountInput`:

```json
{"bytes":22,"lines":3,"path":"poem.txt","report":{"byte_len":12,"id":"word-count.tsv","media_type":"text/tab-separated-values"},"words":5}
```

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "path": {
      "type": "string",
      "description": "Workspace-relative path to count. Resolved through the call's boundary."
    },
    "skip_blank_lines": {
      "type": "boolean",
      "description": "Whether blank lines are left out of the counts.",
      "default": false
    }
  },
  "required": [
    "path"
  ],
  "description": "What the tool accepts. Its JSON Schema is generated from this type.\n\n`deny_unknown_fields` is what closes the published schema. Without it an\nagent's misspelled field is discarded by serde instead of reported, so every\n`Input` type carries it.",
  "title": "WordCountInput",
  "$schema": "https://json-schema.org/draft/2020-12/schema"
}
```

Three things in that schema were not written by hand. `additionalProperties:
false` came from `#[serde(deny_unknown_fields)]`. The `description`s came from
the doc comments. And `required` came from serde's own view of which fields have
defaults — which is the same view the deserializer will use, because it is the
same derive.

## Identity and versions

A tool is keyed by `(id, version)`, and both halves are part of the public
contract.

**The identifier** is lowercase ASCII letters, digits, and underscores, grouped
into dot-separated segments that each start with a letter, with at least a
namespace and a verb: `fs.read`, `workspace.inspect`, `example.word_count`. At
most 64 characters. The grammar is narrow on purpose — every character it admits
survives a shell word, a SQL literal, a JSON key, and a URL path segment
unescaped, so the one identity that ties a descriptor, a `tool_calls` row, and an
approval scope together never needs quoting rules that differ between them.

**The version** is a semantic version, compared by *precedence* rather than as
text, so `0.10.0` follows `0.9.0` and not the other way round. Build metadata is
refused, because it is ignored for precedence and so cannot distinguish one
version from another.

Three rules follow from a recorded call naming a version and expecting it to keep
meaning what it meant:

- **A registration is immutable.** There is no way to replace or remove one.
  Publishing a change means registering a new version *beside* the old one.
- **Resolving without a version selects the highest stable version.** A
  pre-release is chosen only when nothing stable is registered, so registering
  `2.0.0-rc.1` beside a production `1.10.0` does not quietly move every
  unversioned caller onto the candidate.
- **The version that actually ran travels with the result** — in the outcome on
  success, and on the error on failure — so a caller records what executed rather
  than what it asked for. Two lookups can disagree; one cannot.

## Schemas are generated, never declared

Schemas are produced from `Input` and `Output` by `schemars` at *registration*,
and compiled into validators at the same moment. A schema that cannot be compiled
is a registration failure rather than a surprise on the first call.

Nothing here retrieves a schema from outside the process: `jsonschema` is built
without its `resolve-http` and `resolve-file` features, so a `$ref` to a URL or a
local file is refused at registration rather than fetched. The draft
meta-schemas ship inside `jsonschema`, so a `$ref` to one resolves from its
built-in registry with nothing retrieved.

**One thing is the author's responsibility.** `schemars` closes an object schema
— emits `additionalProperties: false` — only when the type carries
`#[serde(deny_unknown_fields)]`. Without it the published schema is open and an
unexpected key is silently discarded by serde rather than refused. An agent that
misspells a field name should be told, not quietly ignored, and the published
schema is what tells it. **Put `#[serde(deny_unknown_fields)]` on every `Input`
type.**

Both validation gates locate their findings. A violation carries an RFC 6901 JSON
Pointer into the offending value *and* another into the schema rule it broke,
which is what makes a refusal actionable for an agent retrying on its own:

```text
fs.read@1.0.0 input does not satisfy its declared schema: /path: 42 is not of type "string"
```

Everything on that path is bounded, because all of it derives from
caller-supplied data: at most a fixed number of violations with the true number
of omissions stated, each *field* truncated — the pointer as well as the
explanation — and the whole projection finally clamped to
`MAX_FAILURE_MESSAGE_BYTES`. That last bound is the one that matters most. It is
not schema-specific: it is the guarantee that *any* failure, including a tool
flattening a verbose cause or a panic payload quoting its own input, fits the run
store's inline payload limit. A failure too large to record would leave the call
stuck in `running` with no account of why, which is worse than a failure
described in less detail.

## Risk and capabilities

A tool declares its `RiskLevel` once, and the registry never rewrites it. See
[Policy](policy.md#the-six-risk-levels) for what the six levels admit.

Pick the level by asking what executing the tool *can* affect, not what a typical
call does. `process.exec` is `execute` even when the program it is handed is
`true`, because nothing in this process can enumerate what a program will do.

`Capability` names something a tool needs granted — `fs.write`, `process.spawn`.
The list is sorted and deduplicated in the descriptor, so a declaration order
cannot leak into enumeration and a repeated capability cannot be mistaken for a
stronger requirement. Capabilities matter twice: they are what a
`capability_for_run` approval is matched against by subset, and they are what a
policy rule for an external capability selects on.

Two further declarations are the author's alone, because nothing outside the body
can see them:

- **`spawning_processes()`** — whether calls of this tool start child processes.
  Its one consumer is the scheduler's global process limit. `RiskLevel` is the
  wrong proxy in both directions: a `Network` tool that shells out to Git sits
  *above* `Execute`, and an in-process interpreter could sit at `Execute` while
  spawning nothing. A tool that under-declares is scheduled as though it spawned
  nothing and its children escape the bound; over-declaring only costs it a slot
  it did not need.
- **`with_environment([...])`** — which parent environment variables this tool's
  children may inherit. An arbitrary tool child is unknown code, so the
  environment starts *empty* and copies only `PATH`, `HOME`, `LANG`, `LC_ALL`,
  `TERM`, plus whatever is declared here. Names are validated and canonicalized
  to uppercase ASCII, because Windows environment lookup is case-insensitive and
  two spellings of one variable would let a differently-cased declaration
  retrieve a value policy never saw.

## Declaring the effects of one call

`request_effects` runs on *schema-valid typed input*, before policy and before
execution. It is where a tool says what this particular call will touch — and it
must not perform the operation.

```rust
fn request_effects(
    &self,
    input: &Self::Input,
    boundary: &PathBoundary,
) -> Result<RequestEffects, ToolError> {
    let path = boundary.contain(&input.path)?;
    Ok(RequestEffects::default().with_path(path, PathAccess::Read))
}
```

Two kinds of fact go in.

**Paths, with their access mode.** `boundary.contain` returns a `ContainedPath`,
a capability that cannot be constructed unchecked; a path that escapes the
workspace is refused here rather than misclassified as an ordinary workspace
write. The mode maps to a risk floor: `Read` → `observe`, `Write` →
`workspace_write`, `Destructive` → `destructive`.

**Non-filesystem flags**, through `RequestFlags`: `executing()`,
`using_network()`, `writing_remote()`, `destructive()`, and
`force_pushing(variant)`. Forcing *implies* remote write, so a caller cannot
describe a force push as something less consequential by omitting the
remote-write flag.

The result is folded into a `RequestClassification`, which can only be produced
by `trust::classify_request` and is floored again at the descriptor's declared
risk. **Effects can only raise the level of a call, never lower it.**

## The execution context

`ExecutionContext` is what a running tool is given. It carries the call's
identity, its boundary, its cancellation token, its deadline, its progress sink,
and its artifact writer.

| Method | Use it for |
| --- | --- |
| `resolve(path)` | Turning a caller-supplied path into a `ContainedPath`. Relative paths start at the workspace root; absolute ones are accepted only when they resolve inside it. |
| `check_still_permitted()` | The check to make between units of work: cancellation *and* deadline, with cancellation reported first. |
| `check_cancelled()` | Cancellation alone. |
| `report(event)` | One progress event. |
| `write_artifact(name, media_type, bytes)` | Content the tool already holds. |
| `open_artifact(name, media_type)` | Content that arrives as a stream. |
| `redact_text(text)` | Bounded inline text, through the same policy as this call's artifacts. |
| `workspace_root()`, `boundary()` | The call's filesystem extent. |
| `workspace_metadata()` | Catalog identity — **`Option`**, because a direct invocation is valid without catalog state. A tool must report the absence rather than inventing a project id from the path. |
| `execution_mode()` | Whether a child may attach to a terminal. |
| `run()`, `step()`, `call()` | The identifiers this call is recorded under. |

Never bypass `resolve`. Joining a caller string onto `workspace_root()` yourself
skips the canonical containment check, which is the whole point of the boundary.

## Timeouts and cancellation

Every call has a way to end. The tool declares what bounds it; a caller may
replace a limit with any finite one but **may never remove the bound**.

| Declaration | Meaning | Default for |
| --- | --- | --- |
| `ToolMetadata::within(d)` | Killed once `d` of wall-clock time has passed. | — |
| `ToolMetadata::bounded_only_by_cancellation()` | Runs until it finishes or its token is cancelled. | `network`, `remote_write` |
| (nothing) | `ToolTimeout::for_risk(risk)` — 30 s for `observe`, 120 s for `workspace_write`, `execute` and `destructive`. | everything else |

Declaring `bounded_only_by_cancellation` is a *claim by the author* that the body
is stoppable: that it polls `check_still_permitted` or hands its token to
something that does. Nothing can verify that claim, which is precisely why it has
to be declared rather than assumed.

The executor runs the body on its own thread, so a hang becomes a `TimedOut`
outcome rather than a wait with no end. Rust cannot kill a thread, so an
unstoppable body is *abandoned* — its call is recorded terminal and the thread is
left. A child process is not abandoned: `ToolProcess` kills its whole process
group.

A body that notices first stops sooner and gets to unwind its own work, which is
why the loop in the example polls once per line rather than trusting the outside
enforcement.

**One-way irreversibility.** A tool whose commit phase must not be interrupted
half-way — an atomic file replacement, say — enters an irreversible phase, after
which the executor will not abandon the worker but waits for the body's real
outcome. Persisting `cancelled` while the body can still commit bytes would make
the durable record disagree with the workspace. Enter it exactly once,
immediately before a *bounded* commit phase, after every validation; entering
deliberately trades the escape hatch for an honest record.

## Progress

`ProgressEvent` is the typed generalization of the `impl FnMut(String)` callback
Git operations take. That callback can only say *something happened*; naming the
three shapes separately means a consumer can render a bar, a phase label, and a
log line without pattern-matching English.

| Constructor | Renders as |
| --- | --- |
| `ProgressEvent::message(text)` | a log line |
| `ProgressEvent::stage(name)` | a phase label |
| `ProgressEvent::counted(done, total, unit)` | a determinate bar (`fraction()` is clamped) |
| `ProgressEvent::indeterminate(done, unit)` | a spinner with a count |

Units are `bytes`, `files`, `objects`, `items`.

Progress travels over a **bounded** channel, so a tool reporting faster than the
log can record *waits* rather than growing a queue: progress describes work in
flight, so a queue that grows without bound reports the past while consuming
memory in the present. Every event becomes a `tool_progress` entry in the run
log. The window folds consecutive `tool_progress` events of one call into a
single timeline row that counts its updates — presentation only; nothing leaves
the log, and `harkness run show` still prints every tick.

Reporting is not a failure path: dropping the receiver is not an error, and a
consumer that has given up leaves the tool free to run to its own conclusion.

## Artifacts

Output travels through schema validation and is persisted inline under a 64 KiB
bound. **Anything that can grow with the input belongs in an artifact**, with only
a reference in the result.

`ArtifactRef` derives `JsonSchema` precisely so a tool can return one *inside* its
`Output` without hand-writing a schema or mirroring the struct — which would
reintroduce exactly the type/schema divergence this layer exists to prevent.

`write_artifact` is for content already in hand. `open_artifact` returns an
`io::Write` for content that arrives as a stream, and that is the whole write
surface: there is no method taking a whole artifact, so no caller can be tempted
into holding a build log in memory. Bytes pass through the redactor, then a
hasher and a counter, then the file.

A tool that cannot store its artifact has not completed its work and should
report the failure rather than return a partial result.

One name is worth knowing: a result the *output* schema refused is itself stored,
under `REJECTED_OUTPUT_ARTIFACT` (`rejected-output.json`), so the error saying
*where* the value broke its schema has the value itself beside it.

## Errors

Return `ToolError`. Every variant carries a stable `kind()` discriminant, which
is what a front end, the policy engine, and a persisted failure all branch on —
never a Rust type. The namespace separates three questions on purpose:

| Why the value was wrong | Why the work did not happen | The tool misbehaving |
| --- | --- | --- |
| `invalid_input`, `invalid_output` | `denied`, `cancelled`, `interrupted`, `timed_out` | `execution_failed`, `tool_panicked` |

plus the ones that name a concrete condition: `process_failed`,
`forbidden_path`, `not_found`, `output_budget_exhausted`,
`outside_allowed_roots`, `symlink_escapes`, `root_unavailable`,
`candidate_unavailable`, `stale_patch`, `patch_conflict`.

A caller deciding whether to retry, to re-prompt, or to stop needs those three
groups apart, so reach for the most specific variant rather than
`execution_failed` with prose. `ToolError::happened_before_execution` is true of
`invalid_input` and deliberately of nothing else.

**Panics are contained.** The tool body is the only foreign code in the pipeline
and it runs under `catch_unwind`; a panic becomes `tool_panicked` carrying the
payload text when it was a string, and the registry and the calling thread stay
usable, so one buggy tool cannot tear down the coordinator and orphan a run
record. Two limits are worth stating: a contained panic *ends* that call rather
than resuming it, because the context is left in whatever state the body
abandoned it in; and an abort — `panic = "abort"`, a failed allocation, a
`process::exit` — is not a panic and is not containable.

## Child processes

`ToolProcess` is what a tool that shells out uses. It generalizes
`harkness-git`'s runner: its own process group so cancellation kills the whole
tree, both pipes drained concurrently, a 20 ms poll honouring the call's deadline
and token, and each output stream streamed into an artifact while only a bounded
tail is retained in memory for the failure message.

Two rules go with it:

- **argv only, never a shell.** Each argument is passed literally, so a path with
  spaces or metacharacters stays one argument.
- **The environment is an allowlist, not a denylist.** It starts empty and copies
  only the baseline plus what the descriptor declared. Git is deliberately
  different — it runs one known program and preserves most of its caller's
  environment so credential helpers keep working — and the two models sit beside
  each other because their trust assumptions are different.

Declare `spawning_processes()`, and see
[Filesystem and process safety](filesystem-and-process-safety.md).

## Registering it

```rust
let mut registry = ToolRegistry::new();
registry.register(WordCount)?;
```

Registration is where the schemas are generated and compiled, so this is where a
broken contract fails. `register` returns `RegistryError`:
`invalid_tool_id`, `invalid_tool_version`, `invalid_capability`,
`invalid_metadata`, `invalid_schema`, `duplicate_registration`. Enumeration is
ordered by identifier and then by version precedence, so generated documentation
and the `harkness contract` projection are diff-stable regardless of registration
order.

The nine production tools are registered by `tools::register_read_only_tools` and
`tools::register_mutating_tools`, and both front ends build the same registry
from them. To see what is published:

<!-- verified -->
```sh
harkness --json tool list
harkness --json tool describe fs.read
```

`describe` includes both generated schemas; `list` omits them, because publishing
nine schema documents in a listing buries the listing.

And to run one through the whole pipeline — resolution, validation, policy,
recording, execution — with no model involved:

<!-- verified -->
```sh
harkness --json tool invoke fs.read --input '{"path":"src/lib.rs"}' --project ws
```

That is not a bypass. It records a task, a run, a step, a tool call, an event log
and any artifacts exactly as an agent's call records them, and the call is
readable afterwards with `harkness run show`.

## The checklist

- [ ] `#[serde(deny_unknown_fields)]` on the `Input` type.
- [ ] Doc comments on every input and output field — they become the published
      `description`s, and they are what a person reads before approving.
- [ ] A `title` under 120 characters and a `description` under 2048, neither
      blank.
- [ ] The `RiskLevel` chosen by what the tool *can* affect.
- [ ] Every `Capability` the tool needs, declared.
- [ ] `spawning_processes()` if it starts a child.
- [ ] `with_environment([...])` for anything a child needs beyond the baseline.
- [ ] `request_effects` resolving every path argument through the boundary, with
      the right `PathAccess`.
- [ ] A timeout that is either the risk default or a deliberate override.
- [ ] `check_still_permitted()` between units of work.
- [ ] Anything unbounded written to an artifact rather than returned inline.
- [ ] The most specific `ToolError` variant, not `execution_failed` with prose.
- [ ] A new **version** rather than an edit, if any of the above changes for a
      tool that has already shipped.

## What proves this

| Claim | Package | Test |
| --- | --- | --- |
| The documented example is the file it claims to be | `harkness-runtime` | `the_tool_authoring_example_is_the_file_it_claims_to_be` |
| Schema-invalid input is refused before the body runs | `harkness-runtime` | `tool::tests::schema_invalid_input_is_refused_before_the_tool_body_runs` |
| A registry lookup meets its 1 ms budget | `harkness-runtime` | `tool::tests::registry_lookup_meets_the_latency_target` |
| Per-call dispatch overhead stays inside its budget | `harkness-runtime` | `tools::read_tests::registry_lookup_and_dispatch_overhead_stay_within_issue_budgets` |
| A nonzero exit becomes `process_failed` with a bounded tail | `harkness-runtime` | `tool::execution_tests::processes::a_nonzero_exit_becomes_process_failed_with_a_bounded_stderr_tail` |
| A hanging child is killed with its whole process group | `harkness-runtime` | `tool::execution_tests::processes::a_hanging_child_is_killed_at_its_timeout_with_its_whole_process_group` |
| An undeclared parent variable is not inherited | `harkness-runtime` | `tools::tests::process_exec_does_not_inherit_an_undeclared_parent_canary` |
| Shell metacharacters stay single arguments | `harkness-runtime` | `tools::tests::process_exec_preserves_shell_metacharacters_as_single_arguments` |
| An escaping symlink is refused by read and search | `harkness-runtime` | `tools::read_tests::escaping_symlinks_are_refused_by_read_and_search` |
| `tool list` and `describe` publish every descriptor and both schemas | `harkness-cli` | `tool_list_and_describe_publish_every_descriptor_and_both_schemas` |
| `tool invoke` executes without an agent and returns typed output | `harkness-cli` | `tool_invoke_executes_without_an_agent_and_returns_validated_typed_output` |
| A schema violation names the field | `harkness-cli` | `tool_invoke_input_violating_the_published_schema_is_a_usage_error_naming_the_field` |
