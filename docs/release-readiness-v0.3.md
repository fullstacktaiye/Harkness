# The v0.3 release gate

The v0.3 epic states its definition of done as a list of objectively
demonstrable properties. This document is where each one is checked and its
evidence written down, so that "done" is a decision somebody can audit rather
than a summary of which pull requests merged.

Every criterion below names the test that proves it, and those names are
re-derived from the test binaries by
`.github/scripts/verify-doc-references.sh`. A criterion whose test is renamed or
deleted fails a job rather than quietly becoming a sentence that used to be
true. That is the same bargain `docs/verification-suite.md` makes for the
release-blocking scenarios, and it is the reason this document is in the tree
instead of only in the issue that produced it.

- [What this document is, and is not](#what-this-document-is-and-is-not)
- [The tree this gate audits](#the-tree-this-gate-audits)
- [One engine, and the tools it runs](#one-engine-and-the-tools-it-runs)
- [Policy and approval](#policy-and-approval)
- [Persistence and interruption](#persistence-and-interruption)
- [The agent and the flagship workflow](#the-agent-and-the-flagship-workflow)
- [Concurrency and the window](#concurrency-and-the-window)
- [The pipeline and the documents](#the-pipeline-and-the-documents)
- [Release mechanics](#release-mechanics)
- [The child issues](#the-child-issues)
- [What was deferred, and where it went](#what-was-deferred-and-where-it-went)

## What this document is, and is not

It is an audit record. It implements nothing and fixes nothing: a criterion
without producible evidence is a gap that reopens the issue which owes it, and a
criterion met by something outside the default pipeline is recorded as exactly
that rather than rounded up.

One of the eighteen criteria — that the Qt main thread stays responsive during
runs — is met by evidence a hosted runner cannot produce today. The performance
confirmation the gate also owes is taken on one platform rather than three. Both
are named in full below rather than smoothed over, and neither is a property
that fails: they are missing *coverage in CI*, and each has a follow-up issue
(#235, #236).

## The tree this gate audits

The audited tree is `4085186`, the tip of `main` when this gate ran, plus this
gate's own commit — which changes the workspace version, adds this document and
`CHANGELOG.md`, and extends two documentation checks. Nothing in it changes
runtime behavior, so every result below holds for both.

`main` already carries merged v0.4 and v0.5 work: the context engine, the shared
JSON-RPC transport, the provider contract, the ACP client handshake, and the
external-agent registry. None of it is required by, or reachable from, the v0.3
criteria — the v0.3 surface is complete underneath it, which is what this gate
checks.

The full workspace suite and the three documentation checks were run on the
audited tree, and the pipeline was green on `4085186` across every job,
including the self-hosted one the gate does not require:

| Job | Runner | Result |
| --- | --- | --- |
| `lint` | `ubuntu-latest` | green |
| `core` | `ubuntu-latest`, `macos-latest`, `windows-latest` | green |
| `verification-suite` | `ubuntu-latest` | green |
| `latency` | `ubuntu-latest`, release profile | green |
| `github-remote-import` | self-hosted | green, and not required by this gate |

## One engine, and the tools it runs

**The window and the command line drive the same engine.** They are compared as
two readers over one store rather than as one reader with itself: the
equivalence test executes a run through the real `harkness` binary and reads it
back through the window's own loaders.

**Every registered tool has a stable identity and a generated contract.**
Schemas are derived from a tool's `Input` and `Output` types rather than
declared beside them, so a descriptor cannot publish a contract that disagrees
with the type its body deserializes; a schema that will not compile fails
registration instead of the first call. Error kinds live in two namespaces that
are checked not to collide.

| Criterion | Package | Test |
| --- | --- | --- |
| The window and the command line report one store identically | `harkness-gui` | `front_end_equivalence::the_window_and_the_command_line_report_the_same_runs_states_and_events` |
| A second tool claiming a registered identity is rejected | `harkness-runtime` | `tool::tests::duplicate_tool_id_and_version_registration_is_rejected` |
| Every descriptor carries generated input and output schemas | `harkness-runtime` | `tool::tests::registered_descriptors_carry_generated_schemas_for_input_and_output` |
| A schema that cannot compile fails registration, not the first call | `harkness-runtime` | `tool::tests::a_schema_that_cannot_be_compiled_fails_registration_not_the_first_call` |
| The two error namespaces are a union without collision | `harkness-runtime` | `tool::error::tests::invocation_error_kinds_are_the_two_namespaces_without_collision` |
| A tool is usable with no agent, policy, or store in the process | `harkness-runtime` | `tool::tests::a_tool_invokes_directly_without_agent_policy_or_store` |

## Policy and approval

**No execution path reaches a tool body without a recorded decision.** Policy is
evaluated before execution and its verdict and reason are persisted with the
call. Repository configuration layers over user policy and may only tighten it:
the weakening check is exhaustive over the inputs a repository can supply, not a
sample of them.

**A grant authorizes one request and not its successor.** An approval binds tool
id, tool version, canonical input hash, workspace, run, scope and lifecycle
status. The two halves are both needed and neither implies the other: one proves
a single changed byte produces a different hash, the other proves the runtime
notices at dispatch and refuses rather than executing under a grant that no
longer describes the call.

| Criterion | Package | Test |
| --- | --- | --- |
| An allowed call records its policy decision before it executes | `harkness-runtime` | `coordinator::tests::allowed_call_records_policy_before_execution` |
| The built-in table covers every risk and trust pair | `harkness-runtime` | `policy::tests::built_in_table_covers_every_risk_and_trust_branch` |
| No repository policy input can lower any verdict | `harkness-runtime` | `policy::tests::no_repository_policy_input_can_lower_any_verdict` |
| A grant is bound to the exact call before execution | `harkness-runtime` | `coordinator::tests::ask_grant_is_bound_to_the_exact_call_before_execution` |
| A denial is bound to the call and never executes it | `harkness-runtime` | `coordinator::tests::ask_denial_is_bound_to_the_call_and_never_executes_it` |
| An approved call records the decision beside the version it authorized | `harkness-runtime` | `tool::execution_tests::an_approved_call_runs_and_records_the_decision_beside_the_version_it_authorized` |
| Input tampered while parked is refused at dispatch | `harkness-runtime` | `coordinator::tests::approved_dispatch_rejects_input_tampered_while_parked` |
| One changed byte of one field changes the hash | `harkness-runtime` | `approval::canonical::tests::changing_one_byte_of_one_field_changes_the_hash` |

The adversarial security criteria the gate re-verifies specifically — path
escape by `..` and by symlink, environment leakage, shell metacharacters left
inert, approval rebinding, and repository policy that cannot weaken — are the
`Security` table of [the verification suite](verification-suite.md#security),
and every one of them runs in the default `cargo test --workspace` invocation
rather than behind a flag.

## Persistence and interruption

**Everything a run produced outlives the process that produced it.** The
command-line flagship answers both approvals over a pipe and then re-reads the
run in a *second* invocation, which is what makes the last step a claim about
durability rather than about a value still in memory.

**Interruption is detected rather than inferred.** A coordinator holds an
advisory lease; construction sweeps every run whose claim is provably dead and
marks it, its unfinished steps, its in-flight calls and its unanswered approvals
`interrupted`, each with its own appended event. That sweep is the only writer
of the state, which is what makes it mean "the owning process stopped". The
recovery tests kill a child harness process with `SIGKILL` while a tool call is
in flight and assert on the reopened store.

**Large output is streamed to an artifact and never accumulated.** The bound is
enforced in three places, and the store refuses an oversized inline payload at
the threshold rather than writing it and hoping.

| Criterion | Package | Test |
| --- | --- | --- |
| A run survives process exit and reads back from a later invocation | `harkness-cli` | `the_flagship_scenario_runs_end_to_end_and_is_reproducible_from_the_log` |
| A killed process leaves everything it started marked on the next start | `harkness-runtime` | `coordinator::tests::recovery::killing_a_process_mid_run_makes_the_next_start_mark_everything_it_left` |
| Retrying an interrupted run creates a fresh attempt with provenance | `harkness-runtime` | `coordinator::tests::recovery::retrying_an_interrupted_run_creates_a_fresh_attempt_with_provenance` |
| Streamed stdout lands in an artifact while memory stays bounded | `harkness-runtime` | `tool::execution_tests::processes::streamed_stdout_lands_in_an_artifact_while_memory_stays_bounded` |
| An oversized inline payload is refused at the threshold | `harkness-runtime` | `store::column::tests::oversized_inline_payloads_are_refused_at_the_threshold` |
| Oversized artifact metadata is refused before anything is streamed | `harkness-runtime` | `store::tests::oversized_artifact_metadata_is_refused_before_anything_is_streamed` |
| A missing artifact file degrades rather than corrupting the run | `harkness-runtime` | `store::tests::a_missing_artifact_file_degrades_to_availability_missing` |
| A v1 database migrates to current and still reads its existing runs | `harkness-runtime` | `store::tests::a_v1_database_migrates_to_current_and_still_reads_its_existing_runs` |
| A schema newer than this build is refused as an upgrade | `harkness-runtime` | `store::tests::a_newer_schema_is_refused_as_upgrade_and_leaves_the_file_untouched` |

## The agent and the flagship workflow

**The mock agent has no privileged path.** It replays its scripts through the
same `Agent` trait a real agent will implement, with no handle on the registry,
policy, approvals, store, scheduler or execution context — and its inputs go
through the real registry, where an invalid one is refused before any body runs.

**The flagship is proven three times, once per way a user can reach it.** The
twelve steps the epic names — start a task, create a run, plan, inspect status,
read a file, ask to modify, apply the patch once granted, ask to run the
configured test, run it, produce a diff artifact, complete, and show the whole
timeline from a process that did not record it — are covered by
[the flagship table](verification-suite.md#the-flagship-workflow).

| Criterion | Package | Test |
| --- | --- | --- |
| Every scenario replays through the production agent trait | `harkness-runtime` | `every_scenario_replays_its_complete_action_sequence_through_the_agent_trait` |
| Scenario inputs go through the real registry and are refused there | `harkness-runtime` | `invalid_tool_input_is_rejected_by_the_real_registry_before_the_body_runs` |
| Session state round-trips through the real run event store | `harkness-runtime` | `session_state_round_trips_through_the_real_run_event_store` |
| All ten scenarios are registered in a stable order | `harkness-runtime` | `all_ten_scenarios_are_registered_in_stable_order` |
| Two replays produce identical actions and history digests | `harkness-runtime` | `agent::mock::tests::identical_replays_produce_identical_actions_and_history_digests` |
| The flagship runs end to end and re-reads from a second process | `harkness-cli` | `the_flagship_scenario_runs_end_to_end_and_is_reproducible_from_the_log` |
| The production tools complete the flagship edit, test and diff run | `harkness-runtime` | `coordinator::tests::production_tools_complete_the_flagship_edit_test_diff_run` |

## Concurrency and the window

**Mutation serializes per workspace; reads do not.** The scheduler keys a slot
on `(ProjectId, canonical root)`, admits one mutating call per workspace, and
caps concurrent reads rather than serializing them.

**Run history is inspectable from both front ends after a restart**, and the
window reads it from a store it did not write.

The Qt-thread criterion is the first of the two whose evidence is not fully in
hosted CI, and it is worth stating precisely. The discipline is enforced at
runtime by `assert_off_qt_thread`, a debug assertion armed the first time a
bridge queues work back to the Qt thread; a store or coordinator call made on
that thread aborts the process rather than blocking the window. Arming it needs
a real `exec()`, which means `crates/harkness-gui/tests/run_surfaces.rs` — and
that binary loads Kirigami, which hosted Ubuntu runners do not package for KDE
Frameworks 6. It therefore runs on a developer machine with the Fedora setup
`README.md` describes, and what CI keeps is the compile (`lint`'s `--all-targets`
Clippy pass) plus the listable model and settlement tests below. The gap is
coverage, not the property, and it is filed rather than left implicit — see
[what was deferred](#what-was-deferred-and-where-it-went).

| Criterion | Package | Test |
| --- | --- | --- |
| Two mutations of one workspace run strictly in sequence | `harkness-runtime` | `schedule::tests::two_mutations_of_one_workspace_run_strictly_in_sequence` |
| Reads of one workspace run concurrently up to the cap | `harkness-runtime` | `schedule::tests::reads_of_one_workspace_run_concurrently_up_to_the_cap` |
| Cancellation reaches an executing tool and terminalizes the run | `harkness-runtime` | `coordinator::tests::cancellation_reaches_an_executing_tool_and_terminalizes_the_run` |
| A seeded store reads back newest first with each run's task title | `harkness-gui` | `run_list_model::tests::a_seeded_store_reads_back_newest_first_with_each_run_s_task_title` |
| A read reports what is recorded and corrects nothing | `harkness-gui` | `run_list_model::tests::a_read_reports_what_is_recorded_and_corrects_nothing` |
| An interrupted run still names the call that was in flight | `harkness-gui` | `runs_backend::tests::an_interrupted_run_still_names_the_call_that_was_in_flight` |
| The four answer properties are counted apart | `harkness-gui` | `runs_backend::tests::the_four_answer_properties_are_counted_apart` |
| A load superseded by another of its own kind writes nothing | `harkness-gui` | `runs_backend::tests::a_load_superseded_by_another_of_its_own_kind_writes_nothing` |

## The pipeline and the documents

**The default suite reaches no model, no API key, no network service, and no
GitHub account.** The four tests that do reach GitHub are `#[ignore]`d and named
one at a time by the self-hosted job through
`.github/scripts/run-ignored-exact-test.sh`, which fails loudly if a named test
no longer exists. No hosted job in `.github/workflows/network-integration.yml`
reads a secret, and the gate does not require the self-hosted job to be green —
it happened to be, on the audited commit.

**The scenario matrix cannot be satisfied by deleting a row.**
`.github/scripts/verify-suite-mapping.sh` checks the document against the test
binaries *and* against a mandated list that lives in the script, so a scenario
the milestone requires cannot be covered by removing its row from the document.

**The documentation is executed, mirrored, resolved and re-derived.** Four
mechanisms, each holding a different failure that would otherwise be silent.

| Criterion | Package | Test |
| --- | --- | --- |
| Every documented command runs as written, with its exit code asserted | `harkness-cli` | `the_documented_commands_run_as_written` |
| The worked example is the file it claims to mirror | `harkness-runtime` | `the_tool_authoring_example_is_the_file_it_claims_to_be` |
| Every repository path the documentation cites exists | `harkness-runtime` | `every_repository_path_the_documentation_cites_exists` |
| Every link between documents resolves to a file and a heading | `harkness-runtime` | `every_link_between_documents_resolves_to_a_file_and_a_heading` |

The fifth mechanism is not a test, because it needs `cargo test -- --list`:
`.github/scripts/verify-doc-references.sh` re-derives every package and test
named by a "What proves this" table — including every table in this document —
from the binaries themselves.

The v0.3 documentation set is complete and each document is reachable from the
`README.md` table: [the runtime](architecture-runtime.md),
[writing a tool](tool-authoring.md), [policy](policy.md),
[approvals](approvals.md),
[run lifecycle and storage](run-lifecycle-and-storage.md),
[the mock-agent scenarios](mock-agent-scenarios.md),
[diagnostics and redaction](observability.md), and
[the verification suite](verification-suite.md).

## Release mechanics

**The workspace version is `0.3.0`.** It was `0.1.0`, and had never tracked a
milestone number. The owner's decision, recorded here, is to bump it: the v0.3
contract is complete in this tree, and a workspace that has finished three
milestones describing itself as `0.1.0` is a version string nobody can use. The
later work already merged on `main` is additive and unreleased, so the number
names the contract this gate audited rather than everything the tree contains.

**Release notes are `CHANGELOG.md`**, which starts at `0.3.0` — the first
version the manifest names. The v0.1 and v0.2 milestones shipped before the
workspace tracked a version at all, and inventing numbers for them after the
fact would be a release record nobody could check. The GitHub Release is
published from that text.

**The window's screenshots are current.** `README.md` embeds the runs list, an
executing run with folded progress, a failed run, a run awaiting a decision, the
approval review page, a lapsed request, a refused request, and an interrupted
run. They are regenerated by setting `HARKNESS_RUN_SCREENSHOT_DIR` and running
`crates/harkness-gui/tests/run_surfaces.rs`, which changes no assertion — so the
images and the surfaces they show cannot drift apart by hand.

**Milestone 3 is closed** with every issue resolved.

## The child issues

Twenty-one child issues, #85 through #105, all closed and merged. **None was
descoped**, so there is no removal to justify here and no annotation owed to the
epic's checklist beyond marking it complete.

| Phase | Issues |
| --- | --- |
| Contracts and persistence | #85 domain model, #86 SQLite foundation, #87 tool contract, #88 events and artifacts, #89 execution semantics |
| Enforcement | #90 trust and safety, #91 policy evaluator, #92 durable approvals, #93 scheduling and cancellation |
| Orchestration | #94 read-only tools, #95 mutating and process tools, #96 agent interface and mock agent, #97 run coordinator, #98 interrupted-run recovery, #99 command-line commands |
| Native interface | #100 Qt bridge models, #101 runs and timeline, #102 approval and cancellation |
| Verification and release | #103 tracing and redaction, #104 verification suite, #105 documentation, #106 this gate |

## What was deferred, and where it went

Two gaps are recorded rather than fixed here, because this gate implements
nothing. Both are coverage gaps in the hosted pipeline, and neither is a v0.3
property that fails.

**The Kirigami surface tests run on a developer machine, not in CI** (#235).
`crates/harkness-gui/tests/qml_smoke.rs` and
`crates/harkness-gui/tests/run_surfaces.rs` load Kirigami, and hosted Ubuntu
runners package only the KF5 one. They are the checks that arm the Qt-thread
assertion and that regenerate the screenshots, so leaving them uncovered means a
regression in either would be caught by a contributor rather than by a job.
Moving them needs a runner with KDE Frameworks 6 on it, which is an
infrastructure change and not a code one.

**The latency budgets are measured on one machine** (#236). The `latency` job runs them
in release on `ubuntu-latest`, which is the only profile and platform where the
numbers bind; `macos` and `windows` record no measurement at all. The budgets
are documented with the machine beside each number for exactly this reason, but
a platform-specific regression would not fail a job.

## What proves this

| Claim | Package | Test |
| --- | --- | --- |
| Every command this document's siblings show runs as written | `harkness-cli` | `the_documented_commands_run_as_written` |
| Every repository path this document cites exists | `harkness-runtime` | `every_repository_path_the_documentation_cites_exists` |
| Every link this document makes resolves to a file and a heading | `harkness-runtime` | `every_link_between_documents_resolves_to_a_file_and_a_heading` |
