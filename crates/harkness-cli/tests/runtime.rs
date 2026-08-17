//! Black-box coverage of the `run`, `approvals`, `tool`, and `agent` families.
//!
//! Every test drives the real `harkness` binary against a `HARKNESS_DATA_DIR`
//! temporary directory: no network, no GitHub account, and no personal Git
//! configuration. The scenarios that execute a child process name bare
//! executables the fixture harness installs as links to *this* test binary, so
//! nothing depends on a host toolchain either.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use harkness_core::{Project, ProjectService};
use harkness_test_fixtures::Fixture;
use serde_json::Value;
use tempfile::TempDir;

// Declares the ignored child roles the mock-agent process scenarios
// re-execute this binary in.
harkness_test_fixtures::scenario_process_fixture_tests!();

/// Exactly the bytes the flagship scenario's base precondition names.
const FLAGSHIP_SOURCE: &str = "pub const VALUE: &str = \"old\";\n";

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[test]
fn run_list_is_empty_and_creates_no_run_store_before_anything_has_run() {
    let world = World::new();

    let output = world.harkness(&["--json", "run", "list"]);

    assert_success(&output);
    let data = &json_output(&output)["data"];
    assert_eq!(data["kind"], "run_list");
    assert_eq!(data["runs"].as_array().unwrap().len(), 0);
    assert_eq!(data["next_cursor"], Value::Null);
    // A read must not bring `runtime.db` into existence: a caller that only
    // reported would otherwise write to a data directory it was asked to read.
    assert!(!world.data_dir().join("runtime.db").exists());
}

#[test]
fn run_list_pages_newest_first_through_an_opaque_cursor_that_round_trips() {
    let world = World::new();
    // Four runs: the one `--trust-workspace` records, then three scenarios.
    let mut recorded = vec![world.trust()];
    for _ in 0..3 {
        recorded.push(world.read_only_run());
    }
    recorded.reverse();

    let whole = world.harkness(&["--json", "run", "list"]);
    assert_success(&whole);
    let whole = json_output(&whole)["data"].clone();
    assert_eq!(ids(&whole["runs"]), recorded, "listing is newest first");
    assert_eq!(whole["next_cursor"], Value::Null);

    let first = world.harkness(&["--json", "run", "list", "--limit", "2"]);
    assert_success(&first);
    let first = json_output(&first)["data"].clone();
    let cursor = first["next_cursor"]
        .as_str()
        .expect("two runs remain unreturned");
    let second = world.harkness(&["--json", "run", "list", "--limit", "2", "--cursor", cursor]);
    assert_success(&second);
    let second = json_output(&second)["data"].clone();

    // The cursor is opaque, and continuing from it neither repeats nor skips.
    assert_eq!(ids(&first["runs"]), recorded[..2]);
    assert_eq!(ids(&second["runs"]), recorded[2..]);
    assert_eq!(second["next_cursor"], Value::Null);
    assert_eq!(first["runs"][0]["state"], "succeeded");
    assert_eq!(
        first["runs"][0]["task_title"],
        "Agent scenario read_only_success"
    );
}

#[test]
fn a_run_page_limit_over_the_published_cap_is_a_usage_error() {
    let world = World::new();

    for limit in ["0", "501"] {
        let output = world.harkness(&["--json", "run", "list", "--limit", limit]);
        assert_eq!(output.status.code(), Some(2), "--limit {limit}");
        assert_eq!(json_output(&output)["error"]["kind"], "usage_error");
    }
    let refused = world.harkness(&["--json", "run", "list", "--cursor", "not-a-token"]);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(json_output(&refused)["error"]["kind"], "usage_error");
}

#[test]
fn run_show_reports_the_run_its_calls_approvals_artifacts_and_a_paged_timeline() {
    let world = World::new();
    world.trust();
    let run = world.read_only_run();

    let output = world.harkness(&["--json", "run", "show", &run, "--limit", "3"]);

    assert_success(&output);
    let data = &json_output(&output)["data"];
    assert_eq!(data["kind"], "run_show");
    assert_eq!(data["run"]["id"], run.as_str());
    assert_eq!(data["run"]["state"], "succeeded");
    // `retry_of` and `workspace_may_be_modified` are always reported, so a
    // caller never has to tell "not a retry" from "an older producer".
    assert_eq!(data["run"]["retry_of"], Value::Null);
    assert_eq!(data["run"]["workspace_may_be_modified"], false);
    assert_eq!(data["task"]["title"], "Agent scenario read_only_success");
    assert_eq!(data["steps"].as_array().unwrap().len(), 3);
    let calls = data["tool_calls"].as_array().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| (
                call["tool_id"].as_str().unwrap(),
                call["state"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("workspace.inspect", "succeeded"),
            ("fs.read", "succeeded"),
            ("git.diff", "succeeded"),
        ]
    );
    assert!(calls[0]["policy_decision"]["verdict"] == "allow");
    // The timeline is a page, not the whole log, and it continues by sequence.
    let events = data["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(data["order"], "oldest");
    assert_eq!(data["next_cursor"], 3);

    let newest = world.harkness(&[
        "--json", "run", "show", &run, "--limit", "2", "--order", "newest",
    ]);
    assert_success(&newest);
    let newest = json_output(&newest)["data"].clone();
    let seqs = newest["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(seqs[0] > seqs[1], "a newest-first page reads backwards");
    assert_eq!(newest["next_cursor"], seqs[1]);
}

#[test]
fn showing_a_run_that_was_never_recorded_reports_not_found() {
    let world = World::new();
    world.trust();
    world.read_only_run();

    let output = world.harkness(&[
        "--json",
        "run",
        "show",
        "00000000-0000-4000-8000-000000000009",
    ]);

    assert_eq!(output.status.code(), Some(4));
    let error = &json_output(&output)["error"];
    // The runtime's own spelling, not a CLI-invented `run_not_found`. What the
    // record was travels in the details instead.
    assert_eq!(error["kind"], "not_found");
    assert_eq!(error["details"]["record"], "run");
    assert_eq!(
        error["details"]["id"],
        "00000000-0000-4000-8000-000000000009"
    );
}

#[test]
fn cancelling_a_run_this_process_is_not_driving_is_refused_by_name() {
    let world = World::new();
    world.trust();
    let run = world.read_only_run();

    let output = world.harkness(&["--json", "run", "cancel", &run]);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(json_output(&output)["error"]["kind"], "run_not_active");
    let unknown = world.harkness(&[
        "--json",
        "run",
        "cancel",
        "00000000-0000-4000-8000-000000000009",
    ]);
    assert_eq!(unknown.status.code(), Some(4));
    assert_eq!(json_output(&unknown)["error"]["kind"], "not_found");
}

#[test]
fn retrying_records_a_new_attempt_that_names_the_run_it_follows() {
    let world = World::new();
    world.trust();
    let denied = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "approval_denied",
        "--project",
        "ws",
    ]);
    assert_eq!(denied.status.code(), Some(3));
    let original = json_output(&denied)["error"]["details"]["run_id"]
        .as_str()
        .expect("a denied run still names itself")
        .to_owned();

    let output = world.harkness(&[
        "--json",
        "run",
        "retry",
        &original,
        "--scenario",
        "read_only_success",
    ]);

    assert_success(&output);
    let data = &json_output(&output)["data"];
    assert_eq!(data["kind"], "run_retry");
    assert_eq!(data["retry_of"], original.as_str());
    assert_eq!(data["run"]["retry_of"], original.as_str());
    assert_eq!(data["run"]["state"], "succeeded");
    assert_ne!(data["run"]["id"], original.as_str());
    // The earlier attempt's write was denied before it started, so nothing on
    // disk can be attributed to it.
    assert_eq!(data["run"]["workspace_may_be_modified"], false);

    // Both attempts are readable, and the original is untouched apart from the
    // one line naming its successor.
    let shown = world.harkness(&["--json", "run", "show", &original, "--limit", "1000"]);
    assert_success(&shown);
    let shown = json_output(&shown)["data"].clone();
    assert_eq!(shown["run"]["state"], "failed");
    assert!(
        shown["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "run_retried"),
        "the original timeline records the retry"
    );
}

#[test]
fn retrying_a_run_that_succeeded_is_refused_with_the_runtimes_own_kind() {
    let world = World::new();
    world.trust();
    let run = world.read_only_run();

    let output = world.harkness(&[
        "--json",
        "run",
        "retry",
        &run,
        "--scenario",
        "read_only_success",
    ]);

    assert_eq!(output.status.code(), Some(3));
    let error = &json_output(&output)["error"];
    assert_eq!(error["kind"], "run_not_retryable");
    assert_eq!(error["details"]["state"], "succeeded");
}

// ---------------------------------------------------------------------------
// approvals
// ---------------------------------------------------------------------------

#[test]
fn approvals_list_reports_answered_requests_with_their_tool_and_recorded_input() {
    let world = World::new();
    world.trust();
    let denied = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "approval_denied",
        "--project",
        "ws",
    ]);
    assert_eq!(denied.status.code(), Some(3));

    // The pending queue is empty: the request was answered before the run
    // ended, which is what a noninteractive invocation does with every one.
    let pending = world.harkness(&["--json", "approvals", "list"]);
    assert_success(&pending);
    assert_eq!(
        json_output(&pending)["data"]["approvals"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let all = world.harkness(&["--json", "approvals", "list", "--all"]);
    assert_success(&all);
    let approvals = json_output(&all)["data"]["approvals"].clone();
    let approvals = approvals.as_array().unwrap();
    assert_eq!(approvals.len(), 1);
    let approval = &approvals[0];
    assert_eq!(approval["tool_id"], "fs.apply_patch");
    assert_eq!(approval["tool_version"], "1.0.0");
    assert_eq!(approval["risk"], "workspace_write");
    assert_eq!(approval["state"], "denied");
    assert_eq!(approval["decision"]["verdict"], "denied");
    assert_eq!(approval["decision"]["decided_via"], "cli");
    assert!(
        approval["input_summary"]
            .as_str()
            .unwrap()
            .contains("fs.apply_patch"),
        "a readable summary is reported"
    );
    // The recorded call's own input, as persisted through the redactor.
    assert!(approval["input"]["patch"].is_string());
    assert_eq!(approval["input_hash"].as_str().unwrap().len(), 64);

    // Every decision is visible from the run it belongs to as well.
    let run = approval["run_id"].as_str().unwrap();
    let shown = world.harkness(&["--json", "run", "show", run]);
    assert_success(&shown);
    assert_eq!(
        json_output(&shown)["data"]["approvals"][0]["decision"]["verdict"],
        "denied"
    );
}

#[test]
fn answering_an_approval_no_live_run_is_parked_on_is_refused() {
    let world = World::new();
    world.trust();
    let denied = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "approval_denied",
        "--project",
        "ws",
    ]);
    assert_eq!(denied.status.code(), Some(3));
    let all = world.harkness(&["--json", "approvals", "list", "--all"]);
    let approvals = json_output(&all)["data"]["approvals"].clone();
    let approval = approvals[0]["id"].as_str().unwrap().to_owned();

    // Deciding wakes a thread parked in the process that started the run, and
    // this invocation started none. An already-answered request is refused for
    // that reason before its own lifecycle is ever consulted.
    for verb in ["approve", "deny"] {
        let output = world.harkness(&["--json", "approvals", verb, &approval]);
        assert_eq!(output.status.code(), Some(3), "{verb}");
        assert_eq!(
            json_output(&output)["error"]["kind"],
            "approval_not_active",
            "{verb}"
        );
    }
    let malformed = world.harkness(&["--json", "approvals", "approve", "not-an-id"]);
    assert_eq!(malformed.status.code(), Some(2));
    assert_eq!(json_output(&malformed)["error"]["kind"], "usage_error");
}

// ---------------------------------------------------------------------------
// tool
// ---------------------------------------------------------------------------

#[test]
fn tool_list_and_describe_publish_every_descriptor_and_both_schemas() {
    let world = World::new();

    let listed = world.harkness(&["--json", "tool", "list"]);
    assert_success(&listed);
    let tools = json_output(&listed)["data"]["tools"].clone();
    let tools = tools.as_array().unwrap();
    let ids = tools
        .iter()
        .map(|tool| tool["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "descriptors are ordered by identifier");
    for expected in [
        "check.run",
        "fs.apply_patch",
        "fs.read",
        "git.diff",
        "git.status",
        "process.exec",
        "test.run",
        "workspace.inspect",
        "workspace.search",
    ] {
        assert!(ids.iter().any(|id| id == expected), "{expected} is missing");
    }
    // A listing publishes the contract, not the schemas: nine schema documents
    // would bury the listing.
    assert!(tools[0].get("input_schema").is_none());

    let described = world.harkness(&["--json", "tool", "describe", "fs.read"]);
    assert_success(&described);
    let data = &json_output(&described)["data"];
    assert_eq!(data["tool"]["id"], "fs.read");
    assert_eq!(data["tool"]["version"], "1.0.0");
    assert_eq!(data["tool"]["risk"], "observe");
    assert_eq!(data["tool"]["spawns_processes"], false);
    assert!(data["tool"]["input_schema"]["properties"]["path"].is_object());
    assert!(data["tool"]["output_schema"]["properties"]["content"].is_object());
    assert_eq!(data["versions"][0], "1.0.0");

    let unknown = world.harkness(&["--json", "tool", "describe", "fs.nonexistent"]);
    assert_eq!(unknown.status.code(), Some(4));
    assert_eq!(json_output(&unknown)["error"]["kind"], "unknown_tool");
    let stale = world.harkness(&[
        "--json",
        "tool",
        "describe",
        "fs.read",
        "--tool-version",
        "9.9.9",
    ]);
    assert_eq!(stale.status.code(), Some(4));
    assert_eq!(json_output(&stale)["error"]["kind"], "unknown_tool_version");
    // Describing the contract touches no data directory at all.
    assert!(!world.data_dir().join("runtime.db").exists());
}

#[test]
fn tool_invoke_executes_without_an_agent_and_returns_validated_typed_output() {
    let world = World::new();
    world.trust();

    let output = world.harkness(&[
        "--json",
        "tool",
        "invoke",
        "fs.read",
        "--input",
        "{\"path\":\"src/lib.rs\"}",
        "--project",
        "ws",
    ]);

    assert_success(&output);
    let data = &json_output(&output)["data"];
    assert_eq!(data["kind"], "tool_invoke");
    let call = &data["tool_call"];
    assert_eq!(call["tool_id"], "fs.read");
    assert_eq!(call["tool_version"], "1.0.0");
    assert_eq!(call["state"], "succeeded");
    assert_eq!(call["output"]["content"], FLAGSHIP_SOURCE);
    assert_eq!(call["output"]["content_encoding"], "utf8");
    assert_eq!(call["policy_decision"]["verdict"], "allow");

    // The invocation is auditable: its recorded call is the one `run show`
    // reports for the run the envelope names.
    let run = data["run_id"].as_str().unwrap();
    let shown = world.harkness(&["--json", "run", "show", run]);
    assert_success(&shown);
    assert_eq!(
        json_output(&shown)["data"]["tool_calls"][0]["id"],
        call["id"]
    );
}

#[test]
fn tool_invoke_reads_its_input_document_from_standard_input() {
    let world = World::new();
    world.trust();

    let output = world.harkness_with_stdin(
        &[
            "--json",
            "tool",
            "invoke",
            "fs.read",
            "--input",
            "-",
            "--project",
            "ws",
        ],
        "{\"path\":\"src/lib.rs\"}\n",
    );

    assert_success(&output);
    assert_eq!(
        json_output(&output)["data"]["tool_call"]["output"]["content"],
        FLAGSHIP_SOURCE
    );
}

#[test]
fn tool_invoke_input_violating_the_published_schema_is_a_usage_error_naming_the_field() {
    let world = World::new();
    world.trust();

    let output = world.harkness(&[
        "--json",
        "tool",
        "invoke",
        "fs.read",
        "--input",
        "{\"path\":42}",
        "--project",
        "ws",
    ]);

    assert_eq!(output.status.code(), Some(2));
    let error = &json_output(&output)["error"];
    assert_eq!(error["kind"], "invalid_input");
    assert!(
        error["message"].as_str().unwrap().contains("/path"),
        "the offending field is named: {}",
        error["message"]
    );
    // Malformed JSON never reaches the registry at all.
    let malformed = world.harkness(&[
        "--json",
        "tool",
        "invoke",
        "fs.read",
        "--input",
        "{",
        "--project",
        "ws",
    ]);
    assert_eq!(malformed.status.code(), Some(2));
    assert_eq!(json_output(&malformed)["error"]["kind"], "usage_error");
}

#[test]
fn tool_invoke_of_a_workspace_write_tool_is_denied_noninteractively_and_writes_nothing() {
    let world = World::new();
    world.trust();
    let before = fs::read(world.root().join("src/lib.rs")).unwrap();

    let output = world.harkness(&[
        "--json",
        "tool",
        "invoke",
        "fs.apply_patch",
        "--input",
        &world.flagship_patch_input(),
        "--project",
        "ws",
    ]);

    assert_eq!(output.status.code(), Some(3));
    let error = &json_output(&output)["error"];
    assert_eq!(error["kind"], "approval_required_noninteractive");
    assert_eq!(error["details"]["tool_call"]["state"], "denied");
    assert_eq!(
        error["details"]["approvals"][0]["decision"]["verdict"],
        "denied"
    );
    assert_eq!(
        fs::read(world.root().join("src/lib.rs")).unwrap(),
        before,
        "a denied call must not have executed"
    );
}

#[test]
fn an_untrusted_workspace_refuses_before_a_run_is_recorded() {
    let world = World::new();

    let output = world.harkness(&[
        "--json",
        "tool",
        "invoke",
        "fs.read",
        "--input",
        "{\"path\":\"src/lib.rs\"}",
        "--project",
        "ws",
    ]);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        json_output(&output)["error"]["kind"],
        "confirmation_required"
    );
    let listed = world.harkness(&["--json", "run", "list"]);
    assert_eq!(
        json_output(&listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "nothing is recorded for a workspace nobody vouched for"
    );
}

// ---------------------------------------------------------------------------
// agent
// ---------------------------------------------------------------------------

#[test]
fn agent_scenarios_lists_every_script_this_build_replays() {
    let world = World::new();

    let output = world.harkness(&["--json", "agent", "scenarios"]);

    assert_success(&output);
    let names = json_output(&output)["data"]["scenarios"].clone();
    assert!(
        names
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "edit_test_diff_success")
    );
    let unknown = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "nope",
        "--project",
        "ws",
    ]);
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(json_output(&unknown)["error"]["kind"], "usage_error");
}

#[test]
fn agent_run_streams_progress_on_stderr_and_prints_one_result_on_stdout() {
    let world = World::new();
    world.trust();

    let output = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "read_only_success",
        "--project",
        "ws",
    ]);

    assert_success(&output);
    let data = &json_output(&output)["data"];
    assert_eq!(data["kind"], "agent_run");
    assert_eq!(data["run"]["state"], "succeeded");
    let run = data["run_id"].as_str().unwrap();

    // Standard error is one JSON object per line, each an envelope-v1 progress
    // record carrying the persisted event it reports.
    let progress = String::from_utf8(output.stderr.clone()).unwrap();
    let lines = progress
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON object per line"))
        .collect::<Vec<_>>();
    assert!(!lines.is_empty());
    for line in &lines {
        assert_eq!(line["v"], 1);
        assert_eq!(line["type"], "progress");
    }
    // The envelope says whether the live stream was complete, so the assertion
    // is that it *was* rather than an assumption that it always is.
    assert_eq!(data["timeline_complete"], true, "the live stream was lost");
    let events = lines
        .iter()
        .filter(|line| !line["event"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), lines.len(), "every line carried its event");
    for line in &events {
        assert_eq!(line["event"]["run_id"], run);
    }
    assert_eq!(events.len() as u64, data["event_count"].as_u64().unwrap());
    assert_eq!(
        events.last().unwrap()["event"]["seq"],
        data["last_event_seq"]
    );

    // A follow-up read reproduces the same timeline from the durable log.
    let shown = world.harkness(&["--json", "run", "show", run, "--limit", "1000"]);
    assert_success(&shown);
    let replayed = json_output(&shown)["data"]["events"]
        .as_array()
        .expect("the timeline is a list")
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    let streamed = events
        .into_iter()
        .map(|line| line["event"]["seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replayed, streamed);
}

#[test]
fn agent_run_denies_an_unanswerable_approval_and_leaves_the_workspace_alone() {
    let world = World::new();
    world.trust();
    let before = fs::read(world.root().join("src/lib.rs")).unwrap();

    let output = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "approval_denied",
        "--project",
        "ws",
    ]);

    assert_eq!(output.status.code(), Some(3));
    let error = &json_output(&output)["error"];
    assert_eq!(error["kind"], "approval_required_noninteractive");
    assert_eq!(error["details"]["run"]["state"], "failed");
    assert_eq!(
        fs::read(world.root().join("src/lib.rs")).unwrap(),
        before,
        "no answer is refusal, and a refusal executes nothing"
    );
}

#[test]
fn interactive_mode_denies_when_standard_input_closes_without_an_answer() {
    let world = World::new();
    world.trust();

    let output = world.harkness_with_stdin(
        &[
            "--json",
            "agent",
            "run",
            "--scenario",
            "approval_denied",
            "--project",
            "ws",
            "--interactive",
        ],
        "",
    );

    assert_eq!(output.status.code(), Some(1));
    let error = &json_output(&output)["error"];
    // Not `approval_required_noninteractive`: a terminal was offered and the
    // caller declined to use it, which the run records as an ordinary denial.
    assert_eq!(error["kind"], "run_failed");
    assert_eq!(
        error["details"]["approvals"][0]["decision"]["reason"],
        "standard input closed before the approval was answered"
    );
    assert_eq!(error["details"]["tool_calls"][0]["state"], "denied");
}

#[test]
fn interactive_mode_denies_on_an_explicit_answer_and_records_who_decided() {
    let world = World::new();
    world.trust();

    let output = world.harkness_with_stdin(
        &[
            "--json",
            "agent",
            "run",
            "--scenario",
            "approval_denied",
            "--project",
            "ws",
            "--interactive",
        ],
        "show-input\ndeny\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let error = &json_output(&output)["error"];
    let decision = &error["details"]["approvals"][0]["decision"];
    assert_eq!(decision["verdict"], "denied");
    assert_eq!(decision["decided_via"], "cli");
    assert_eq!(decision["reason"], "denied on the Harkness command line");
    // The prompt, the shown input and the help all went to standard error.
    let progress = String::from_utf8(output.stderr).unwrap();
    assert!(
        progress.contains("approve (this call only)"),
        "the line protocol announces that the bare answer is the narrow one"
    );
    assert!(
        progress.contains("base_sha256"),
        "show-input reveals the recorded input"
    );
}

#[test]
fn the_flagship_scenario_runs_end_to_end_and_is_reproducible_from_the_log() {
    let world = World::new();
    world.trust();

    // Two approvals: the workspace write, then the process execution. A
    // trusted workspace still asks for both, which is the whole point.
    let output = world.harkness_with_stdin(
        &[
            "--json",
            "agent",
            "run",
            "--scenario",
            "edit_test_diff_success",
            "--project",
            "ws",
            "--interactive",
        ],
        "approve\napprove\n",
    );

    assert_success(&output);
    let data = &json_output(&output)["data"];
    assert_eq!(data["scenario"], "edit_test_diff_success");
    assert_eq!(data["scenario_version"], 2);
    assert_eq!(data["run"]["state"], "succeeded");
    let calls = data["tool_calls"].as_array().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| call["tool_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "workspace.inspect",
            "fs.read",
            "fs.apply_patch",
            "test.run",
            "git.diff",
        ]
    );
    assert!(calls.iter().all(|call| call["state"] == "succeeded"));
    // The edit really happened, and the test really ran.
    assert_eq!(
        fs::read_to_string(world.root().join("src/lib.rs")).unwrap(),
        "pub const VALUE: &str = \"new\";\n"
    );
    assert_eq!(calls[3]["output"]["passed"], true);
    let artifacts = data["artifacts"].as_array().unwrap();
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact["media_type"] == "text/x-diff"),
        "the patch's diff is captured as an artifact"
    );
    for artifact in artifacts {
        assert_eq!(artifact["availability"], "available");
        assert_eq!(artifact["sha256"].as_str().unwrap().len(), 64);
        assert!(artifact["byte_size"].is_number());
    }

    let run = data["run_id"].as_str().unwrap();
    let shown = world.harkness(&["--json", "run", "show", run, "--limit", "1000"]);
    assert_success(&shown);
    let shown = json_output(&shown)["data"].clone();
    assert_eq!(
        shown["events"].as_array().unwrap().len() as u64,
        data["event_count"].as_u64().unwrap()
    );
    assert_eq!(data["timeline_complete"], true);
    assert_eq!(shown["tool_calls"].as_array().unwrap().len(), 5);
    assert_eq!(shown["approvals"].as_array().unwrap().len(), 2);
    for approval in shown["approvals"].as_array().unwrap() {
        assert_eq!(approval["decision"]["verdict"], "granted");
        // A bare `approve` authorizes the call in front of the reader and
        // nothing else, even though the stored request would have permitted the
        // tool for the rest of the run.
        assert_eq!(approval["decision"]["scope"], "exact_call");
        assert_eq!(approval["requested_scope"], "tool_for_run");
    }
}

#[test]
fn a_wider_grant_has_to_be_asked_for_by_name() {
    let world = World::new();
    world.trust();

    let output = world.harkness_with_stdin(
        &[
            "--json",
            "agent",
            "run",
            "--scenario",
            "approval_denied",
            "--project",
            "ws",
            "--interactive",
        ],
        "approve-tool\n",
    );

    // The scenario scripts a denial and diverges when the call succeeds, so the
    // run fails — what is being asserted is the recorded breadth of the grant.
    assert!(!output.status.success());
    let approvals = json_output(&output)["error"]["details"]["approvals"].clone();
    assert_eq!(approvals[0]["decision"]["verdict"], "granted");
    assert_eq!(approvals[0]["decision"]["scope"], "tool_for_run");
}

#[test]
fn answering_an_approval_never_brings_a_run_store_into_existence() {
    let world = World::new();

    let output = world.harkness(&[
        "--json",
        "approvals",
        "approve",
        "00000000-0000-4000-8000-000000000001",
    ]);

    assert_eq!(output.status.code(), Some(4));
    let error = &json_output(&output)["error"];
    assert_eq!(error["kind"], "not_found");
    assert_eq!(error["details"]["record"], "approval");
    // A mistyped identifier must not create the schema, or the next `run list`
    // stops taking the "no database means the empty projection" path.
    assert!(!world.data_dir().join("runtime.db").exists());
}

#[test]
fn a_piped_input_document_and_an_interactive_prompt_cannot_share_standard_input() {
    let world = World::new();
    world.trust();

    let output = world.harkness_with_stdin(
        &[
            "--json",
            "tool",
            "invoke",
            "fs.read",
            "--input",
            "-",
            "--project",
            "ws",
            "--interactive",
        ],
        "{\"path\":\"src/lib.rs\"}\napprove\n",
    );

    // Refused by name rather than ending as "standard input closed before the
    // approval was answered", which is a denial whose real cause is a conflict.
    assert_eq!(output.status.code(), Some(2));
    let message = json_output(&output)["error"]["message"].clone();
    assert_eq!(json_output(&output)["error"]["kind"], "usage_error");
    assert!(
        message.as_str().unwrap().contains("--interactive"),
        "the refusal names both flags: {message}"
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_agent_run_cancels_the_run_cooperatively_and_exits_130() {
    let world = World::new();
    world.trust();

    // A scenario whose child parks until it is stopped, so the interrupt
    // arrives while real work is in flight rather than racing the run.
    let mut child = world
        .command(&[
            "--json",
            "agent",
            "run",
            "--scenario",
            "user_cancellation",
            "--project",
            "ws",
            "--interactive",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"approve\n").unwrap();
    stdin.flush().unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let mut seen = Vec::new();
    for line in stderr.lines() {
        let line = line.unwrap();
        let event = serde_json::from_str::<Value>(&line).expect("one JSON object per line");
        seen.push(line);
        if event["event"]["kind"] == "tool_call_state_changed"
            && event["event"]["payload"]["state"] == "running"
        {
            break;
        }
    }
    assert!(
        !seen.is_empty(),
        "the run never reported a call it had started"
    );
    // SAFETY: the child PID is live and belongs to this test process.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0,
        "SIGINT could not be delivered"
    );
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(130));
    let error = &json_output(&output)["error"];
    assert_eq!(error["kind"], "run_cancelled");
    let run = error["details"]["run_id"].as_str().unwrap();

    let shown = world.harkness(&["--json", "run", "show", run, "--limit", "1000"]);
    assert_success(&shown);
    let shown = json_output(&shown)["data"].clone();
    assert_eq!(shown["run"]["state"], "cancelled");
    assert_eq!(shown["tool_calls"][0]["state"], "cancelled");
}

// ---------------------------------------------------------------------------
// contract
// ---------------------------------------------------------------------------

#[test]
fn the_published_contract_names_an_exit_code_for_every_runtime_and_tool_kind() {
    let world = World::new();

    let output = world.harkness(&["--json", "contract"]);

    assert_success(&output);
    let data = &json_output(&output)["data"];
    for namespace in ["runtime", "tool"] {
        let kinds = data["error_kinds"][namespace].as_array().unwrap();
        assert!(!kinds.is_empty(), "{namespace} publishes no kinds");
        for kind in kinds {
            let kind = kind.as_str().unwrap();
            let code = &data["exit_code_by_kind"][namespace][kind];
            assert!(code.is_number(), "{namespace}/{kind} has no exit code");
            assert!(
                [0, 1, 2, 3, 4, 5, 130].contains(&code.as_u64().unwrap()),
                "{namespace}/{kind} reports an unpublished exit code"
            );
        }
    }
    // The kinds this issue introduced are the ones a script keys on.
    for kind in [
        "run_not_active",
        "run_still_active",
        "run_not_retryable",
        "approval_not_active",
    ] {
        assert_eq!(data["exit_code_by_kind"]["runtime"][kind], 3);
    }
    assert_eq!(data["exit_code_by_kind"]["runtime"]["not_found"], 4);
    assert_eq!(data["exit_code_by_kind"]["runtime"]["store_busy"], 5);
    assert_eq!(data["exit_code_by_kind"]["tool"]["invalid_input"], 2);
    assert_eq!(data["exit_code_by_kind"]["tool"]["cancelled"], 130);
    assert_eq!(
        data["exit_code_by_kind"]["cli"]["approval_required_noninteractive"],
        3
    );
    assert_eq!(data["exit_code_by_kind"]["cli"]["run_cancelled"], 130);
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// One hermetic project, its data directory, and the process fixtures the
/// mock-agent scenarios name.
struct World {
    fixture: Fixture,
    workspace: TempDir,
    project: Project,
    path: OsString,
}

impl World {
    fn new() -> Self {
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
        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        let project = service.import_local(&root).unwrap();
        let path = fixture.scenario_process_path();
        Self {
            fixture,
            workspace,
            project,
            path,
        }
    }

    fn data_dir(&self) -> &Path {
        &self.fixture.data_dir
    }

    fn root(&self) -> PathBuf {
        self.project.root.clone()
    }

    /// Records the positive trust decision every run above `Observe` needs.
    ///
    /// Done once through the flag the commands publish, so the tests exercise
    /// the same route a user takes rather than writing the row themselves. The
    /// invocation it takes to do so records a run of its own, which is returned
    /// because a listing test has to account for it.
    fn trust(&self) -> String {
        let output = self.harkness(&[
            "--json",
            "tool",
            "invoke",
            "workspace.inspect",
            "--input",
            "{}",
            "--project",
            "ws",
            "--trust-workspace",
        ]);
        assert_success(&output);
        json_output(&output)["data"]["run_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Runs the read-only scenario and returns the run it recorded.
    fn read_only_run(&self) -> String {
        let output = self.harkness(&[
            "--json",
            "agent",
            "run",
            "--scenario",
            "read_only_success",
            "--project",
            "ws",
        ]);
        assert_success(&output);
        json_output(&output)["data"]["run_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The flagship patch, bound to the exact bytes on disk.
    fn flagship_patch_input(&self) -> String {
        serde_json::to_string(&serde_json::json!({
            "patch": "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub const VALUE: &str = \"old\";\n+pub const VALUE: &str = \"new\";\n",
            "bases": [{
                "path": "src/lib.rs",
                "base_sha256": "4f03383f0bbf9e30e56d77f0a1b85286436cf6df407f00ade9f115b71f382026",
            }],
        }))
        .unwrap()
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_harkness"));
        command
            .env("HARKNESS_DATA_DIR", self.data_dir())
            // Scoped to this child so the frozen bare program names in the
            // scenarios resolve to fixtures rather than to host tools.
            .env("PATH", &self.path)
            .current_dir(self.workspace.path())
            .args(arguments);
        command
    }

    fn harkness(&self, arguments: &[&str]) -> Output {
        self.command(arguments)
            .output()
            .expect("harkness command should start")
    }

    fn harkness_with_stdin(&self, arguments: &[&str], stdin: &str) -> Output {
        let mut child: Child = self
            .command(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("harkness command should start");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(stdin.as_bytes())
            .expect("the answers should be written");
        child.wait_with_output().expect("harkness should finish")
    }
}

fn ids(runs: &Value) -> Vec<String> {
    runs.as_array()
        .unwrap()
        .iter()
        .map(|run| run["id"].as_str().unwrap().to_owned())
        .collect()
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
