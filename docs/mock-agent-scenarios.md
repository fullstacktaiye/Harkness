# The mock-agent scenarios

v0.3 ships no model. What it ships instead is `MockAgent`: a deterministic
replayer of ten frozen scripts, driven through the same seam a model-backed agent
will be driven through. Each script is a list of transitions — one structural
observation pattern and one action — and the mock advances only through
`Agent::next_action`. It receives no registry, policy evaluator, approval gate,
store, scheduler, or execution context, so a scenario cannot bypass a protection
a real agent will face.

Each scenario exists to make one thing observable end to end. What proves the
runtime is the *runtime's* tests; what these prove is that the whole path — agent
to policy to approval to scheduler to executor to store to timeline — behaves as
documented when driven from outside.

- [Running one](#running-one)
- [Preconditions](#preconditions)
- [The ten scenarios](#the-ten-scenarios)
- [Divergence](#divergence)
- [Three that a single invocation cannot converge](#three-that-a-single-invocation-cannot-converge)
- [What a scenario is made of](#what-a-scenario-is-made-of)
- [What proves this](#what-proves-this)

## Running one

<!-- verified -->
```sh
harkness --json agent scenarios
```

```json
{"v":1,"type":"success","ok":true,"data":{"kind":"agent_scenarios","scenarios":[
  "read_only_success","edit_test_diff_success","approval_denied","invalid_tool_input",
  "tool_process_failure","tool_timeout","user_cancellation","restart_recovery",
  "forbidden_path","disallowed_capability"]}}
```

<!-- verified -->
```sh
harkness --json agent run --scenario read_only_success --project ws
```

Every run streams its persisted timeline to standard error as one progress
envelope per event and prints exactly one bounded result envelope on standard
output. The whole timeline is read back afterwards with `harkness run show`.

An unknown name is a usage error at exit 2 with the whole registry listed, rather
than a run that is recorded and then fails.

## Preconditions

**A trusted workspace.** Every scenario needs one, and every scenario above
`observe` still asks for an approval inside it. Record the decision once:

<!-- verified -->
```sh
harkness --json tool invoke workspace.inspect --input '{}' --project ws --trust-workspace
```

**A fixture repository.** Four scenarios name `src/lib.rs`, and two of those bind
its exact bytes through a SHA-256 precondition. The file must be exactly:

```text
pub const VALUE: &str = "old";
```

with a trailing newline —
`4f03383f0bbf9e30e56d77f0a1b85286436cf6df407f00ade9f115b71f382026`. A patch whose
base no longer matches is refused as `stale_patch` without writing anything,
which is the point of the precondition rather than an obstacle to it.

```sh
mkdir -p ws/src && cd ws
git init -b main .
printf 'pub const VALUE: &str = "old";\n' > src/lib.rs
git add -A && git commit -m 'fixture'
```

**Five programs on `PATH`.** The four process scenarios and the flagship name
bare executables rather than host utilities, deliberately: a built-in must never
depend on a POSIX-only tool or on a program a test did not create. Harkness's own
test harness installs them as links to the running integration-test binary. To
run these from a shell, supply your own — anything with the right exit behaviour
will do, because the tools only observe the status:

```sh
mkdir -p ~/harkness-fixtures && cd ~/harkness-fixtures
printf '#!/bin/sh\nexit 0\n'    > fixture-pass
printf '#!/bin/sh\nexit 1\n'    > fixture-fail
printf '#!/bin/sh\nsleep 600\n' > fixture-hang
printf '#!/bin/sh\nsleep 600\n' > fixture-cancellable
printf '#!/bin/sh\nexit 0\n'    > fixture-disallowed
chmod +x fixture-*
export PATH="$PWD:$PATH"
```

`fixture-disallowed` is the one whose body should never run: the scenario asserts
that policy refuses the call before the process starts.

## The ten scenarios

Every outcome below was observed by running the command as written.

### 1. `read_only_success`

**Proves** that a whole run of `observe`-risk work needs no approval at all in a
trusted workspace, and that three different read tools compose into one timeline.

Calls `workspace.inspect`, `fs.read src/lib.rs`, `git.diff --unstaged`, completes.

<!-- verified -->
```sh
harkness --json agent run --scenario read_only_success --project ws
```

```text
exit 0 · run succeeded · workspace.inspect ✓  fs.read ✓  git.diff ✓
```

### 2. `edit_test_diff_success`

**Proves** the flagship workflow: read, ask before writing, apply a patch bound to
the bytes that were approved, ask before executing, run a test, capture the
resulting diff as an artifact, and finish. This is the only scenario at fixture
version **2**; v1 is a released wire form and is retained unchanged.

Two approvals — the workspace write, then the process execution. A trusted
workspace still asks for both, which is the whole point.

<!-- verified -->
```sh
printf 'approve\napprove\n' | harkness --json agent run \
  --scenario edit_test_diff_success --project ws --interactive
```

```text
exit 0 · run succeeded
  workspace.inspect ✓  fs.read ✓  fs.apply_patch ✓  test.run ✓ (passed: true)  git.diff ✓
  approvals: fs.apply_patch (workspace_write) granted at exact_call
             test.run       (execute)         granted at exact_call
  artifacts: applied.patch text/x-diff 177 · test-stdout.log · test-stderr.log
  src/lib.rs now reads: pub const VALUE: &str = "new";
```

Both requests were *asked* at `tool_for_run` and *answered* at `exact_call`,
because a bare `approve` is the narrowest answer.

### 3. `approval_denied`

**Proves** that a refusal stops the work, that the tool call is recorded `denied`
rather than `failed`, and that the agent is told which way the decision went.

<!-- verified: exit=1 -->
```sh
printf 'deny\n' | harkness --json agent run \
  --scenario approval_denied --project ws --interactive
```

```text
exit 1 · error kind run_failed · run failed with kind approval_denied
  fs.apply_patch: denied
```

The run "fails" because the script's own terminal action is `fail_run`, not
because a denial is an error. Run without `--interactive` it converges the same
way and exits 3 with `approval_required_noninteractive`, because there was nobody
to ask — a different route to the same refusal.

### 4. `invalid_tool_input`

**Proves** that a schema-invalid request is refused *before any tool body runs*.
The agent emits `{"path": 42}` verbatim; the registry rejects it against the
published schema.

<!-- verified -->
```sh
harkness --json agent run --scenario invalid_tool_input --project ws
```

```text
exit 0 · run succeeded · fs.read: failed (invalid_input)
```

The run succeeds because observing and reporting the refusal is what the script
set out to do. Invoking the same input directly reports the violation with a JSON
Pointer into the offending field:

```text
fs.read@1.0.0 input does not satisfy its declared schema: /path: 42 is not of type "string"
```

### 5. `tool_process_failure`

**Proves** that a failed *test* is not a failed *tool*. `test.run` reports the
child's status as data; the call itself succeeded.

<!-- verified -->
```sh
printf 'approve\n' | harkness --json agent run \
  --scenario tool_process_failure --project ws --interactive
```

```text
exit 0 · run succeeded
  test.run: succeeded, output {"passed": false, "exit_code": 1, …}
```

### 6. `tool_timeout`

**Proves** that a hanging child is killed at its deadline, through its whole
process group, and that the timeout is reported as structured output rather than
as a hang. The scenario asks for a one-second bound on a program that sleeps.

<!-- verified -->
```sh
printf 'approve\n' | harkness --json agent run \
  --scenario tool_timeout --project ws --interactive
```

```text
exit 0 · run succeeded
  process.exec: succeeded, output {"timed_out": true, "signal": 9, "exit_code": null, "duration_ms": 1006}
```

### 7. `user_cancellation`

**Proves** the cancellation chain reaches the operating system: the run's token
is tripped, the executor cancels the call's own token, and `ToolProcess` kills
the child's process group.

Approve the execution, then press Ctrl-C while the child is running.

```sh
printf 'approve\n' | harkness --json agent run \
  --scenario user_cancellation --project ws --interactive
# … then Ctrl-C
```

```text
exit 130 · error kind run_cancelled
  process.exec: cancelled
  timeline: tool_call_state_changed {"state":"cancelled"}
            step_finished           {"state":"cancelled"}
            run_state_changed       {"state":"cancelled"}
```

*Illustrative* as written, because it needs a signal delivered by hand;
`ctrl_c_during_agent_run_cancels_the_run_cooperatively_and_exits_130` is the same
sequence driven by a test.

### 8. `restart_recovery`

**Proves** that a call the dead process was holding comes back as `interrupted`,
and that the frozen script and the recovery sweep agree on that spelling.

The script expects its first call to fail with kind `interrupted`, which nothing
a single invocation does can produce — see
[below](#three-that-a-single-invocation-cannot-converge).

*Illustrative.*
`coordinator::tests::recovery::the_restart_recovery_script_answers_what_a_recovered_call_records`
drives the real kill, the real sweep, and then the frozen script over the record
it produced, which is what pins the two together.

### 9. `forbidden_path`

**Proves** that a path leaving the workspace is refused by the boundary, with
nothing read and the refusal recorded on the call.

<!-- verified: exit=1 -->
```sh
harkness --json agent run --scenario forbidden_path --project ws
```

```text
exit 1 · error kind run_failed · run failed with kind scenario_divergence
  fs.read: failed
    kind: outside_allowed_roots
    message: ../outside resolves outside the allowed roots ["…/ws"]
```

The boundary refusal is exactly what the scenario is about, and it is recorded on
the call. The *script* then diverges: it names the error kind `forbidden_path`,
while `fs.read`'s `..` refusal is `outside_allowed_roots`. Both are real kinds in
`ToolError::KINDS` — `forbidden_path` is what a symlink component or an
unnameable path produces — and the frozen fixture is a released wire form, so it
is documented here rather than edited. The invariant itself is covered by
`trust::tests::absolute_outside_paths_and_dot_dot_are_refused` and, for the
symlink route, by `tools::read_tests::escaping_symlinks_are_refused_by_read_and_search`.

### 10. `disallowed_capability`

**Proves** that policy refuses a call *before* the process starts, and that the
agent is told a policy denial rather than a tool failure.

It needs a policy that actually denies the capability; with the built-in defaults
alone an approved `process.exec` simply runs, and the script diverges. Either
layer will do — write one, then run it:

```json
{ "version": 2, "tools": { "process.exec": "deny" } }
```

```sh
harkness --json agent run --scenario disallowed_capability --project ws
```

```text
exit 0 · run succeeded
  process.exec: denied
    kind: policy
    message: deny by user_policy rule for tool process.exec
```

A repository policy of `{"version": 2, "risks": {"execute": "deny"}}` converges
the same way and reports `deny by repository_policy rule for risk execute`.

*Illustrative* as written, because the converging run needs a policy file placed
first.

## Divergence

When reality departs from the script, `MockAgent` returns a typed
`scenario_divergence` naming the expected and actual observation kinds, and does
**not** advance its cursor. The run fails; the recorded call keeps its real
outcome.

```json
{"kind":"scenario_divergence","expected":"tool_result","actual":"policy_denied"}
```

That distinction matters when reading a failed scenario: `harkness run show`
reports what the tool actually did, and the run's own failure explains only that
the script did not expect it. `harkness tool invoke` uses the same mechanism —
a one-call scenario — which is why a denied or failed direct invocation reports
the *call's* verdict as the command's outcome and carries the run's
`scenario_divergence` in `details` rather than hiding it.

## Three that a single invocation cannot converge

Being explicit about this is more useful than pretending otherwise. Every script
still replays end to end at the agent seam — that is
`every_scenario_replays_its_complete_action_sequence_through_the_agent_trait`,
which drives all ten. What three of them cannot get from a single `harkness
agent run` is the *condition* their second transition expects.

| Scenario | The condition one invocation cannot create | Where that condition is exercised |
| --- | --- | --- |
| `user_cancellation` | a signal delivered while the child is running | `harkness-cli` · `ctrl_c_during_agent_run_cancels_the_run_cooperatively_and_exits_130` |
| `restart_recovery` | a process killed mid-call, then a fresh start's sweep | `harkness-runtime` · `coordinator::tests::recovery::the_restart_recovery_script_answers_what_a_recovered_call_records` |
| `disallowed_capability` | a policy layer that denies the capability | `harkness-runtime` · `policy::tests::user_and_tool_rules_participate_in_the_same_severity_lattice` |

`forbidden_path` is a fourth case, of a different kind. It reaches the refusal it
is about — the boundary stops the read and the call records
`outside_allowed_roots` — and then diverges on the *spelling*, because the frozen
v1 script names `forbidden_path`. Both are real kinds; the fixture is a released
wire form, so this is written down here rather than edited away.

## What a scenario is made of

A scenario is a versioned document with an id and an ordered list of steps. Every
step is one `expect` pattern and one `action`.

```json
{
  "v": 1,
  "id": "forbidden_path",
  "steps": [
    { "expect": { "kind": "run_started" },
      "action": { "kind": "call_tool", "tool_id": "fs.read", "tool_version": "1.0.0",
                  "input": { "path": "../outside" } } },
    { "expect": { "kind": "tool_failed", "error_kind": "forbidden_path" },
      "action": { "kind": "complete_run",
                  "summary": "Observed and reported the workspace boundary refusal." } }
  ]
}
```

Five rules hold every script to a shape:

- **Patterns select only stable fields.** A pattern names an observation kind and
  may narrow on an error kind, an approval direction, an artifact media type, or a
  substring of the output — never on a record id, which changes per run.
- **`call_tool.input` is a plain JSON value.** The agent may *request* work; only
  the coordinator may validate, authorize, persist, schedule, and execute it. This
  is why `invalid_tool_input` can emit `{"path": 42}` at all, and why adding a
  convenience method that pre-validated an action would be a privileged path a
  future model agent does not have.
- **Exactly the final step is terminal.** Every script ends in `complete_run` or
  `fail_run`, and no earlier step may.
- **Scripts are bounded.** At most 64 steps and 64 KiB.
- **The fixtures are frozen wire evidence.** The ten built-ins are Rust data
  mirrored byte for byte by the JSON under
  `crates/harkness-runtime/src/agent/fixtures/`. Changing an action, a pattern, a
  field, or a spelling means publishing a new version beside v1 — never editing
  what v1 meant. `edit_test_diff_success` is the one that has: v1 and v2 sit side
  by side.

A run's checkpoint carries the scenario's fixture version, a
domain-separated digest of the exact definition, a cursor, and a chained digest of
the observations already consumed. Two replays may have different session ids —
those are not determinism evidence — while identical observation histories must
yield identical actions and digests.

## What proves this

| Claim | Package | Test |
| --- | --- | --- |
| all ten are registered, in a stable order | `harkness-runtime` | `all_ten_scenarios_are_registered_in_stable_order` |
| every script replays its complete action sequence at the seam | `harkness-runtime` | `every_scenario_replays_its_complete_action_sequence_through_the_agent_trait` |
| the Rust data and the frozen JSON are byte-compatible | `harkness-runtime` | `rust_scenarios_and_frozen_json_fixtures_are_byte_compatible` |
| the frozen success actions use the published tool inputs | `harkness-runtime` | `frozen_success_actions_use_the_published_tool_inputs` |
| the process scenarios resolve to hermetic cross-platform children | `harkness-runtime` | `process_scenarios_resolve_to_hermetic_cross_platform_fixture_children` |
| the flagship, end to end and re-read from the log | `harkness-cli` | `the_flagship_scenario_runs_end_to_end_and_is_reproducible_from_the_log` |
| the flagship through the coordinator with production tools | `harkness-runtime` | `coordinator::tests::production_tools_complete_the_flagship_edit_test_diff_run` |
| every script this build replays is listed | `harkness-cli` | `agent_scenarios_lists_every_script_this_build_replays` |
| a run streams progress on stderr and one result on stdout | `harkness-cli` | `agent_run_streams_progress_on_stderr_and_prints_one_result_on_stdout` |
| an unanswerable approval is denied and the workspace is untouched | `harkness-cli` | `agent_run_denies_an_unanswerable_approval_and_leaves_the_workspace_alone` |
| Ctrl-C cancels cooperatively and exits 130 | `harkness-cli` | `ctrl_c_during_agent_run_cancels_the_run_cooperatively_and_exits_130` |
| the recovery sweep and the frozen script agree | `harkness-runtime` | `coordinator::tests::recovery::the_restart_recovery_script_answers_what_a_recovered_call_records` |
| a failed test is not a failed tool | `harkness-runtime` | `tools::tests::test_run_reports_pass_and_failure_without_turning_a_failed_test_into_a_tool_failure` |
| a hanging child is killed with its whole process group | `harkness-runtime` | `tool::execution_tests::processes::a_hanging_child_is_killed_at_its_timeout_with_its_whole_process_group` |
| a `..` path is refused | `harkness-runtime` | `trust::tests::absolute_outside_paths_and_dot_dot_are_refused` |
| schema-invalid input is refused before the body runs | `harkness-runtime` | `tool::tests::schema_invalid_input_is_refused_before_the_tool_body_runs` |
| the real registry refuses `invalid_tool_input`'s value before any body runs | `harkness-runtime` | `invalid_tool_input_is_rejected_by_the_real_registry_before_the_body_runs` |
| a session checkpoint round-trips through the real event store | `harkness-runtime` | `session_state_round_trips_through_the_real_run_event_store` |
