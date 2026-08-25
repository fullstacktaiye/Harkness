# The verification suite

Every component issue proves its own piece. This document is the statement of
what the *composition* is held to: the flagship workflow through both front
ends, the scenario matrix, the migration and security checks, and the latency
targets — each named by the test that proves it, and each checked by
`.github/scripts/verify-suite-mapping.sh` rather than asserted here and left to
rot.

That script is the point. A mapping in prose is true on the day it is written; a
mapping a job re-derives from the test binaries fails loudly the first time a
test named below is renamed or removed, in the same way
`run-ignored-exact-test.sh` fails when a latency target it names disappears. If
you rename a test in the table, update the table in the same commit.

- [Running the suite](#running-the-suite)
- [The flagship workflow](#the-flagship-workflow)
- [The scenario matrix](#the-scenario-matrix)
- [Migration](#migration)
- [Security](#security)
- [Latency targets](#latency-targets)
- [What runs where, and why](#what-runs-where-and-why)

## Running the suite

```sh
cargo test --workspace                     # everything but the ignored targets
sh .github/scripts/verify-suite-mapping.sh # this document against the binaries
```

`cargo test --workspace` is the invocation the suite is written against, and one
test needs it specifically: `front_end_equivalence` drives the real `harkness`
binary, which only a workspace-wide build produces. Running just that crate
works if the binary already exists — `cargo build -p harkness-cli` first, or
point `HARKNESS_CLI_BIN` at one — and says so rather than skipping if it does
not.

Nothing in the suite reaches the network, reads a GitHub credential, or consults
the user's Git configuration. The tests that do reach GitHub are `#[ignore]`d and
run only on the self-hosted job; see [What runs where](#what-runs-where-and-why).

## The flagship workflow

The twelve steps #84 names — start a task, create a run, plan, inspect
`git status`, read a file, ask to modify, apply the patch once granted, ask to
run the configured test, run it, produce a Git diff artifact, complete, and show
the whole timeline from a process that did not record it — are proven three
times over, once per way a user can reach them.

| Scenario | Package | Test |
| --- | --- | --- |
| `flagship-command-line` | `harkness-cli` | `the_flagship_scenario_runs_end_to_end_and_is_reproducible_from_the_log` |
| `flagship-engine` | `harkness-runtime` | `coordinator::tests::production_tools_complete_the_flagship_edit_test_diff_run` |
| `front-end-equivalence` | `harkness-gui` | `front_end_equivalence::the_window_and_the_command_line_report_the_same_runs_states_and_events` |

The command-line test answers both approvals over a pipe and then re-reads the
run in a *second* invocation, which is what makes the last step a claim about
durability rather than about a value still in memory. The engine test drives the
same scenario through `RunCoordinator` with the production tool registry, in a
child process whose `PATH` resolves the scenario's fixture executables. The
equivalence test executes through the command line and reads back through the
window's own loaders, so "the same run" is a comparison of two readers over one
store rather than of one reader with itself.

## The scenario matrix

| Scenario | Package | Test |
| --- | --- | --- |
| `clean-and-dirty-repository` | `harkness-runtime` | `tools::read_tests::workspace_inspect_reports_head_and_dirty_state_or_null_outside_a_repository` |
| `valid-patch` | `harkness-runtime` | `tools::tests::applying_a_matching_patch_atomically_returns_the_resulting_diff_artifact` |
| `invalid-patch` | `harkness-runtime` | `tools::tests::empty_and_malformed_patches_are_conflicts_without_writes` |
| `process-success` | `harkness-runtime` | `tools::tests::test_run_reports_pass_and_failure_without_turning_a_failed_test_into_a_tool_failure` |
| `process-failure` | `harkness-runtime` | `tool::execution_tests::processes::a_nonzero_exit_becomes_process_failed_with_a_bounded_stderr_tail` |
| `process-timeout` | `harkness-runtime` | `tool::execution_tests::processes::a_hanging_child_is_killed_at_its_timeout_with_its_whole_process_group` |
| `user-cancellation` | `harkness-runtime` | `coordinator::tests::cancellation_reaches_an_executing_tool_and_terminalizes_the_run` |
| `user-cancellation` | `harkness-cli` | `ctrl_c_during_agent_run_cancels_the_run_cooperatively_and_exits_130` |
| `approval-granted` | `harkness-runtime` | `tool::execution_tests::an_approved_call_runs_and_records_the_decision_beside_the_version_it_authorized` |
| `approval-denied` | `harkness-cli` | `agent_run_denies_an_unanswerable_approval_and_leaves_the_workspace_alone` |
| `sqlite-lock-contention` | `harkness-runtime` | `store::tests::a_second_store_waits_out_another_connections_write_transaction` |
| `sqlite-lock-contention` | `harkness-runtime` | `store::tests::independent_processes_migrate_a_new_database_exactly_once` |
| `missing-artifact-file` | `harkness-runtime` | `store::tests::a_missing_artifact_file_degrades_to_availability_missing` |
| `interrupted-run-recovery` | `harkness-runtime` | `coordinator::tests::recovery::killing_a_process_mid_run_makes_the_next_start_mark_everything_it_left` |
| `interrupted-run-recovery` | `harkness-runtime` | `coordinator::tests::recovery::retrying_an_interrupted_run_creates_a_fresh_attempt_with_provenance` |
| `concurrent-read-only-calls` | `harkness-runtime` | `schedule::tests::reads_of_one_workspace_run_concurrently_up_to_the_cap` |
| `conflicting-mutating-calls` | `harkness-runtime` | `schedule::tests::two_mutations_of_one_workspace_run_strictly_in_sequence` |
| `paths-with-spaces-and-unicode` | `harkness-runtime` | `tools::tests::a_new_file_with_spaces_unicode_and_no_final_newline_is_created_byte_exactly` |
| `symlink-outside-workspace` | `harkness-runtime` | `trust::tests::a_symlink_pointing_outside_the_workspace_is_refused_by_name` |
| `invalid-run-state-transition` | `harkness-runtime` | `domain::record::tests::every_declared_execution_transition_succeeds_and_every_other_pair_is_invalid` |
| `invalid-tool-call-state-transition` | `harkness-runtime` | `domain::record::tests::every_declared_tool_call_transition_succeeds_and_every_other_pair_is_invalid` |
| `invalid-tool-input` | `harkness-runtime` | `tool::tests::schema_invalid_input_is_refused_before_the_tool_body_runs` |
| `invalid-tool-output` | `harkness-runtime` | `tool::tests::schema_invalid_output_is_refused_before_delivery` |

Two entries are worth reading before trusting them. `interrupted-run-recovery`
kills a *child harness process* with `SIGKILL` while a tool call is in flight and
then asserts on the reopened store — the run `interrupted`, its unfinished work
marked with it, the timeline still readable — which is why the two roles it
re-executes (`park_a_run_awaiting_approval`, `append_event_batches_until_killed`)
are `#[ignore]`d and end in a signal rather than a pass. And
`sqlite-lock-contention` is listed twice on purpose: one connection waiting out
another's transaction and two independent *processes* migrating one new database
are different failures, and neither implies the other.

## Migration

| Scenario | Package | Test |
| --- | --- | --- |
| `migrate-from-frozen-v1` | `harkness-runtime` | `store::tests::a_v1_database_migrates_to_current_and_still_reads_its_existing_runs` |
| `schema-newer-than-supported` | `harkness-runtime` | `store::tests::a_newer_schema_is_refused_as_upgrade_and_leaves_the_file_untouched` |

`crates/harkness-runtime/src/store/fixtures/runtime-v1.db` is the committed
database an earlier build wrote. The upgrade test opens it, climbs the whole
ladder, and re-reads the runs it already held; the probe writes a `user_version`
one past this build's and asserts the refusal carries the dedicated
`schema_too_new` kind and left the file's bytes alone — an upgrade request, not
a corruption report. `AGENTS.md` has the rule the fixtures exist to enforce: a
released migration is never edited, and a new persisted field is a new version
beside the old one.

## Security

These are written adversarially. Each attempts the escape and asserts the
refusal, rather than confirming that the permitted path works.

| Scenario | Package | Test |
| --- | --- | --- |
| `path-escape-dot-dot` | `harkness-runtime` | `trust::tests::absolute_outside_paths_and_dot_dot_are_refused` |
| `path-escape-symlink` | `harkness-runtime` | `tools::read_tests::escaping_symlinks_are_refused_by_read_and_search` |
| `path-escape-symlink` | `harkness-runtime` | `tools::tests::a_patch_targeting_an_escaping_symlink_is_forbidden_without_writing` |
| `environment-leakage` | `harkness-runtime` | `trust::tests::an_allowlisted_child_sees_exactly_the_permitted_environment` |
| `environment-leakage` | `harkness-runtime` | `tools::tests::process_exec_does_not_inherit_an_undeclared_parent_canary` |
| `shell-metacharacters-inert` | `harkness-runtime` | `tools::tests::process_exec_preserves_shell_metacharacters_as_single_arguments` |
| `approval-rebinding` | `harkness-runtime` | `coordinator::tests::approved_dispatch_rejects_input_tampered_while_parked` |
| `approval-rebinding` | `harkness-runtime` | `approval::canonical::tests::changing_one_byte_of_one_field_changes_the_hash` |
| `repository-policy-cannot-weaken` | `harkness-runtime` | `policy::tests::no_repository_policy_input_can_lower_any_verdict` |
| `repository-policy-cannot-weaken` | `harkness-runtime` | `policy::tests::repository_policy_can_raise_and_cannot_lower_a_verdict` |

`approval-rebinding` is the one that needs both halves to mean anything. The
canonicalization test proves a single changed byte produces a different hash;
the dispatch test proves the runtime notices — a call approved, then mutated
while parked, is refused at dispatch with the mismatch persisted rather than
executed under a grant that no longer describes it.

## Latency targets

Every budget below is enforced only where the number means something. The rule
lives in one place — `harkness_test_fixtures::latency::record` — so no target can
drift from it: the measurement is taken and recorded in every profile, and the
threshold binds only when `debug_assertions` is off. Each records one
machine-readable line:

```text
harkness-latency target=store::load_thousand_event_run measured_ns=2323528 budget_ns=500000000 profile=release enforced=true os=linux arch=x86_64 cpus=8
```

They are `#[ignore]`d, so `cargo test --workspace` never runs them. Run one by
name, in release, with output uncaptured:

```sh
sh .github/scripts/run-ignored-exact-test.sh \
    harkness-runtime store::tests::loading_a_thousand_event_run_meets_the_latency_target --release
```

| Scenario | Budget | Package | Test |
| --- | --- | --- | --- |
| `latency-policy-evaluation` | 5 ms | `harkness-runtime` | `policy::tests::policy_evaluation_meets_the_latency_target` |
| `latency-registry-lookup` | 1 ms | `harkness-runtime` | `tool::tests::registry_lookup_meets_the_latency_target` |
| `latency-per-call-overhead` | 10 ms | `harkness-runtime` | `tool::execution_tests::executor_overhead_per_call_meets_the_latency_target` |
| `latency-per-call-overhead` | 10 ms | `harkness-runtime` | `tools::read_tests::registry_lookup_and_dispatch_overhead_stay_within_issue_budgets` |
| `latency-per-call-overhead` | 10 ms | `harkness-runtime` | `per_call_overhead_stays_inside_the_budget_with_the_subscriber_installed` |
| `latency-event-batch-persist` | 10 ms | `harkness-runtime` | `store::tests::persisting_a_state_change_batch_meets_the_latency_target` |
| `latency-event-batch-persist` | 10 ms | `harkness-runtime` | `store::tests::persisting_a_state_change_batch_with_its_events_meets_the_latency_target` |
| `latency-run-list-100` | 100 ms | `harkness-runtime` | `store::tests::listing_one_hundred_runs_meets_the_latency_target` |
| `latency-event-load-1000` | 500 ms | `harkness-runtime` | `store::tests::loading_a_thousand_event_run_meets_the_latency_target` |
| `latency-cancellation-visible` | 250 ms | `harkness-runtime` | `tool::execution_tests::cancellation_latency_meets_the_target` |
| `latency-cancellation-visible` | 250 ms | `harkness-runtime` | `schedule::tests::processes::cancelling_a_run_stops_a_cooperative_child_within_the_promised_latency` |
| `latency-approval-dispatch` | 10 ms | `harkness-runtime` | `coordinator::tests::approval_decision_to_tool_dispatch_stays_below_ten_milliseconds` |
| `latency-streaming-assembly` | 10 µs | `harkness-provider` | `assemble::assembler::tests::event_dispatch_meets_the_latency_target` |
| `latency-inventory-walk` | 1.5 s | `harkness-context` | `inventory::tests::a_medium_repository_meets_the_walk_latency_target` |
| `latency-chunking-1mib` | 20 ms | `harkness-context` | `chunk::tests::chunking_one_megabyte_meets_the_latency_target` |
| `latency-incremental-update` | 1 s | `harkness-context` | `reconcile::tests::a_single_file_update_meets_the_incremental_latency_target` |
| `latency-lexical-search` | 100 ms | `harkness-context` | `search::tests::a_medium_repository_meets_the_content_search_latency_target` |
| `latency-filename-search` | 25 ms | `harkness-context` | `search::tests::a_medium_repository_meets_the_filename_search_latency_target` |

`latency-per-call-overhead` has three entries because the same budget is paid in
three different arrangements, and only one of them is the one that ships: an
executor with no subscriber installed, the read-tool dispatch pipeline, and — the
one that matters — the executor with the real JSON-formatting, redacting,
file-rotating subscriber running. `tracing` is close to free with no subscriber,
so the first two say nothing about the third.

`latency-cancellation-visible` is listed twice for the same reason: a
cooperative tool noticing its token and a child process being killed through its
process group are different chains, and 250 ms is the promise for both.

`latency-lexical-search` is measured on a query that matches **nothing**, so it
opens and scans every eligible file before it can answer. A query that matches
early stops at its result budget after a handful of files and measures almost
nothing, which would make the budget one no repository could ever fail. Both
searches take a capture rather than making one, which is the arrangement a run
uses — a run records one workspace snapshot and stamps every retrieval with it —
and the capture's own cost is printed beside the numbers so the choice hides
nothing.

## What runs where, and why

| Job | Runner | What it holds |
| --- | --- | --- |
| `lint` | `ubuntu-latest` | formatting, Clippy over every target, rustdoc for the crates that deny undocumented items |
| `core` | `ubuntu`, `macos`, `windows` | every crate's tests, per crate, on three platforms |
| `verification-suite` | `ubuntu-latest` | this document and the v0.3 documents against the test binaries, then the window's model tests |
| `latency` | `ubuntu-latest` | every budget above, by exact name, in release |
| `github-remote-import` | self-hosted | the four `#[ignore]`d tests that reach real GitHub |

Three constraints decide that layout.

**The window's tests are split across two jobs, and not by preference.**
`front_end_equivalence` and the model tests link Qt but instantiate no QML, so
they run in `verification-suite`. `qml_smoke` and `run_surfaces` load Kirigami,
and hosted Ubuntu runners package no KDE Frameworks 6 — only the KF5 Kirigami.
Those two therefore run on a developer machine with the Fedora setup `README.md`
describes, and `lint`'s `--all-targets` Clippy pass is what keeps them compiling
in CI. Moving them into CI needs a runner with KF6 on it, not a change here.

**Unix-only scenarios are `#[cfg(unix)]`, not skipped at runtime.** Process
groups, `SIGKILL`, mode-000 paths, symlinks and non-UTF-8 names are capabilities
Windows does not have; the tests that need them are compiled out there rather
than passing vacuously.

**Nothing in the hosted jobs touches the network.** The four tests that do are
`#[ignore]`d and named one at a time by the self-hosted job, through
`run-ignored-exact-test.sh` — which fails if the named test no longer exists, so
renaming one is a change to the workflow file as well.
