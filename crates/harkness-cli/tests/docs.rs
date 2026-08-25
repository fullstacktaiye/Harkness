//! The documented commands, executed as written.
//!
//! `README.md` and the documents under `docs/` show `harkness` invocations. A
//! command in prose is true on the day it is written; a command a test runs
//! fails a build the first time a flag is renamed or an exit code moves. This
//! binary is the second kind.
//!
//! # What is verified
//!
//! A fenced ` ```sh ` or ` ```console ` block **immediately preceded by an
//! `<!-- verified -->` comment** is executed. The comment is invisible in
//! rendered Markdown, so a reader sees an ordinary example and a build sees a
//! test case. Anything unmarked is illustrative — it needs a terminal, a signal,
//! a policy file placed first, or output too long to reproduce — and the
//! document says so where it matters.
//!
//! The marker takes one option:
//!
//! ```text
//! <!-- verified -->            every command in the block exits 0
//! <!-- verified: exit=3 -->    the last command exits 3; the rest still exit 0
//! ```
//!
//! # What a block may contain
//!
//! One command per line, `harkness …`, optionally continued with a trailing
//! `\`. A `console` block may prefix each with `$ `. Two conveniences exist
//! because the documents needed them and a shell does not run here:
//!
//! - `printf '…' | harkness …` feeds the literal to the command's standard
//!   input, so an `--interactive` example shows how it is answered.
//! - `$RUN` expands to the most recent `run_id` any command in the same
//!   *document* reported, so a walkthrough can read back what it just started.
//!
//! There is deliberately no shell. A block that needs one is not a block this
//! runner should be pretending to check.
//!
//! # The world each document runs in
//!
//! One hermetic world per document: a temporary `HARKNESS_DATA_DIR`, a fixture
//! repository named `ws` holding the exact bytes the flagship patch is bound to,
//! and the scenario process fixtures installed under the bare names the frozen
//! scripts contain. `README.md` starts from an empty catalog because its
//! walkthrough imports and trusts the workspace itself; every other document
//! starts from an imported, trusted one, because its examples are about
//! something else.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use harkness_core::{Project, ProjectService};
use harkness_test_fixtures::Fixture;
use serde_json::Value;
use tempfile::TempDir;

// The flagship names `fixture-pass`, which re-executes *this* binary through an
// exact ignored child test. Without these roles `--exact` would match nothing,
// libtest would exit zero having run nothing, and the scenario would observe a
// success from a process that did nothing at all.
harkness_test_fixtures::scenario_process_fixture_tests!();

/// Exactly the bytes the flagship scenario's base precondition names.
const FLAGSHIP_SOURCE: &str = "pub const VALUE: &str = \"old\";\n";

/// Every document whose verified blocks are executed, with the catalog state its
/// examples assume.
const DOCUMENTS: &[(&str, Setup)] = &[
    ("README.md", Setup::Bare),
    ("docs/architecture-runtime.md", Setup::Trusted),
    ("docs/tool-authoring.md", Setup::Trusted),
    ("docs/policy.md", Setup::Trusted),
    ("docs/approvals.md", Setup::Trusted),
    ("docs/run-lifecycle-and-storage.md", Setup::Trusted),
    ("docs/mock-agent-scenarios.md", Setup::Trusted),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Setup {
    /// Nothing is catalogued. The document's own commands import and trust.
    Bare,
    /// The workspace is imported and a positive trust decision is recorded.
    Trusted,
}

#[test]
fn the_documented_commands_run_as_written() {
    let mut executed = 0;
    for (document, setup) in DOCUMENTS {
        let path = repository_root().join(document);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
        let blocks = verified_blocks(&text);
        // Every listed document has to contribute. Without this a renamed marker
        // would leave the document silently unchecked rather than failing.
        assert!(
            !blocks.is_empty(),
            "{document} is listed here but carries no verified block"
        );

        let world = World::new(*setup);
        let mut run_id: Option<String> = None;
        for block in &blocks {
            for (index, command) in block.commands.iter().enumerate() {
                let last = index + 1 == block.commands.len();
                let expected = if last { block.expected_exit } else { 0 };
                let output = world.run(command, run_id.as_deref(), document, block.line);
                assert_exit(&output, expected, command, document, block.line);
                if let Some(observed) = reported_run_id(&output) {
                    run_id = Some(observed);
                }
                executed += 1;
            }
        }
    }
    // A floor rather than an exact count, so adding an example is not a test
    // change — but low enough that only a broken parser could pass it.
    assert!(
        executed >= 20,
        "only {executed} documented commands were executed; the block parser or the markers changed"
    );
}

// ---------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Block {
    /// One-based line of the fence, for a failure message that can be clicked.
    line: usize,
    expected_exit: i32,
    commands: Vec<DocumentedCommand>,
}

#[derive(Debug)]
struct DocumentedCommand {
    /// The command as the document spells it, for failure messages.
    source: String,
    arguments: Vec<String>,
    stdin: Option<String>,
}

/// Every fenced block a `<!-- verified … -->` comment introduces.
fn verified_blocks(document: &str) -> Vec<Block> {
    let lines = document.lines().collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(expected_exit) = marker_exit_code(lines[index]) else {
            index += 1;
            continue;
        };
        // Blank lines between the marker and its fence are ordinary Markdown.
        let mut fence = index + 1;
        while fence < lines.len() && lines[fence].trim().is_empty() {
            fence += 1;
        }
        let opening = lines.get(fence).copied().unwrap_or_default().trim();
        assert!(
            opening == "```sh" || opening == "```console",
            "the verified marker on line {} introduces {opening:?} rather than a shell block",
            index + 1
        );
        let mut body = Vec::new();
        let mut cursor = fence + 1;
        while cursor < lines.len() && lines[cursor].trim() != "```" {
            body.push(lines[cursor]);
            cursor += 1;
        }
        let commands = parse_commands(&body, opening == "```console", fence + 1);
        assert!(
            !commands.is_empty(),
            "the verified block at line {} contains no command",
            fence + 1
        );
        blocks.push(Block {
            line: fence + 1,
            expected_exit,
            commands,
        });
        index = cursor + 1;
    }
    blocks
}

/// The expected exit code of a `<!-- verified … -->` line, if it is one.
fn marker_exit_code(line: &str) -> Option<i32> {
    let body = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    let options = body.strip_prefix("verified")?.trim();
    if options.is_empty() {
        return Some(0);
    }
    let value = options
        .strip_prefix(':')
        .map(str::trim)
        .and_then(|option| option.strip_prefix("exit="))
        .unwrap_or_else(|| panic!("unrecognized verified option {options:?}"));
    Some(
        value
            .parse()
            .unwrap_or_else(|error| panic!("{value:?} is not an exit code: {error}")),
    )
}

fn parse_commands(body: &[&str], console: bool, first_line: usize) -> Vec<DocumentedCommand> {
    let mut commands = Vec::new();
    let mut pending = String::new();
    for (offset, raw) in body.iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = if pending.is_empty() && console {
            match line.strip_prefix("$ ") {
                Some(command) => command,
                // Output lines in a console block are not commands. None of the
                // verified blocks has one, so this is a guard rather than a path.
                None => continue,
            }
        } else {
            line
        };
        if let Some(continued) = line.strip_suffix('\\') {
            pending.push_str(continued.trim_end());
            pending.push(' ');
            continue;
        }
        pending.push_str(line);
        let source = std::mem::take(&mut pending);
        commands.push(parse_command(&source, first_line + offset));
    }
    assert!(
        pending.is_empty(),
        "the block at line {first_line} ends inside a continued command"
    );
    commands
}

fn parse_command(source: &str, line: usize) -> DocumentedCommand {
    let (stdin, invocation) = match split_printf(source) {
        Some((literal, rest)) => (Some(unescape(&literal)), rest),
        None => (None, source.to_owned()),
    };
    let words = shlex::split(&invocation)
        .unwrap_or_else(|| panic!("line {line}: {invocation:?} is not one quoted command"));
    let (program, arguments) = words
        .split_first()
        .unwrap_or_else(|| panic!("line {line}: {invocation:?} names no program"));
    assert_eq!(
        program, "harkness",
        "line {line}: only `harkness` invocations are verified"
    );
    DocumentedCommand {
        source: source.to_owned(),
        arguments: arguments.to_vec(),
        stdin,
    }
}

/// Splits a documented `printf '…' | harkness …` into its literal and the rest.
fn split_printf(source: &str) -> Option<(String, String)> {
    let rest = source.strip_prefix("printf ")?;
    let rest = rest.strip_prefix('\'')?;
    let end = rest.find('\'')?;
    let (literal, remainder) = rest.split_at(end);
    let invocation = remainder[1..].trim_start().strip_prefix('|')?.trim_start();
    Some((literal.to_owned(), invocation.to_owned()))
}

/// Interprets the escapes `printf` would, and nothing else.
fn unescape(literal: &str) -> String {
    let mut text = String::with_capacity(literal.len());
    let mut characters = literal.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('\\') => text.push('\\'),
            Some(other) => {
                text.push('\\');
                text.push(other);
            }
            None => text.push('\\'),
        }
    }
    text
}

// ---------------------------------------------------------------------------
// execution
// ---------------------------------------------------------------------------

fn assert_exit(
    output: &Output,
    expected: i32,
    command: &DocumentedCommand,
    document: &str,
    line: usize,
) {
    let observed = output.status.code();
    assert_eq!(
        observed,
        Some(expected),
        "{document}:{line}: `{}` exited {observed:?}, expected {expected}\nstdout: {}\nstderr: {}",
        command.source,
        String::from_utf8_lossy(&output.stdout),
        tail(&String::from_utf8_lossy(&output.stderr)),
    );
    if !command
        .arguments
        .iter()
        .any(|argument| argument == "--json")
    {
        return;
    }
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{document}:{line}: `{}` did not write one JSON envelope ({error}): {}",
            command.source,
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(
        envelope["v"], 1,
        "{document}:{line}: `{}` reported no envelope version",
        command.source
    );
    let expected_type = if expected == 0 { "success" } else { "error" };
    assert_eq!(
        envelope["type"], expected_type,
        "{document}:{line}: `{}` reported {} on standard output",
        command.source, envelope["type"]
    );
}

/// The `run_id` a result or an error envelope reported, if it named one.
fn reported_run_id(output: &Output) -> Option<String> {
    let envelope: Value = serde_json::from_slice(&output.stdout).ok()?;
    for scope in ["data", "error"] {
        let branch = &envelope[scope];
        for pointer in ["/run_id", "/run/id", "/details/run_id", "/details/run/id"] {
            if let Some(id) = branch.pointer(pointer).and_then(Value::as_str) {
                return Some(id.to_owned());
            }
        }
    }
    None
}

/// The end of a stream, which is where a failure explains itself.
fn tail(text: &str) -> String {
    const KEPT: usize = 4_000;
    if text.len() <= KEPT {
        return text.to_owned();
    }
    let mut start = text.len() - KEPT;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// One hermetic project, its data directory, and the process fixtures the
/// documented scenarios name.
struct World {
    fixture: Fixture,
    workspace: TempDir,
    path: OsString,
}

impl World {
    fn new(setup: Setup) -> Self {
        let fixture = Fixture::new();
        fixture.install_scenario_process_fixtures();
        let workspace = TempDir::new().unwrap();
        let root = workspace.path().join("ws");
        harkness_test_fixtures::initialize_repository(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), FLAGSHIP_SOURCE).unwrap();
        harkness_test_fixtures::commit_all(
            &git2::Repository::open(&root).unwrap(),
            "flagship fixture",
        );
        let path = fixture.scenario_process_path();
        let world = Self {
            fixture,
            workspace,
            path,
        };
        if setup == Setup::Trusted {
            world.import();
            world.trust();
        }
        world
    }

    fn data_dir(&self) -> &Path {
        &self.fixture.data_dir
    }

    fn root(&self) -> PathBuf {
        self.workspace.path().join("ws")
    }

    /// Catalogues the fixture through the service rather than the command line,
    /// so a document whose own first step is `project import` still has one to
    /// perform.
    fn import(&self) -> Project {
        let mut service = ProjectService::load_from_data_dir(self.data_dir()).unwrap();
        service.import_local(self.root()).unwrap()
    }

    /// Records the positive trust decision every run needs, through the flag the
    /// commands publish rather than by writing the row.
    fn trust(&self) {
        let arguments = [
            "--json",
            "tool",
            "invoke",
            "workspace.inspect",
            "--input",
            "{}",
            "--project",
            "ws",
            "--trust-workspace",
        ]
        .map(str::to_owned);
        let command = DocumentedCommand {
            source: "harkness … --trust-workspace".to_owned(),
            arguments: arguments.to_vec(),
            stdin: None,
        };
        let output = self.run(&command, None, "<setup>", 0);
        assert!(
            output.status.success(),
            "recording workspace trust failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run(
        &self,
        command: &DocumentedCommand,
        run_id: Option<&str>,
        document: &str,
        line: usize,
    ) -> Output {
        let arguments = command.arguments.iter().map(|argument| {
            if argument == "$RUN" {
                run_id
                    .unwrap_or_else(|| {
                        panic!("{document}:{line}: $RUN used before any command reported one")
                    })
                    .to_owned()
            } else {
                argument.clone()
            }
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_harkness"))
            .env("HARKNESS_DATA_DIR", self.data_dir())
            // Scoped to this child, so the bare program names frozen in the
            // scenarios resolve to fixtures rather than to host tools.
            .env("PATH", &self.path)
            .current_dir(self.workspace.path())
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("harkness command should start");
        let mut input = child.stdin.take().expect("stdin is piped");
        // Written and then closed either way: a command with no answers to give
        // must still see end of input rather than an open pipe, because an
        // `--interactive` prompt reading a stream nobody closes never returns.
        input
            .write_all(command.stdin.as_deref().unwrap_or_default().as_bytes())
            .expect("the answers should be written");
        drop(input);
        child.wait_with_output().expect("harkness should finish")
    }
}
