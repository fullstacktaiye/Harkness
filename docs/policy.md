# Policy

Policy is the layer that answers one question about one already-validated tool
call: may it proceed, must a human be asked, or is it refused? It is pure — no
I/O, no clock, no lock — so identical loaded policy and identical request values
always produce the same decision, and a decision recorded a year ago can be
re-derived from what was recorded beside it.

Two guardrails are worth reading before anything else, because both are things a
hurried user would otherwise discover by being blocked:

- **A repository can only tighten a decision, never weaken one.**
  `.harkness/policy.json` is repository content, and repository content is
  untrusted (ADR-0006).
- **A question nobody can answer is a refusal.** A headless invocation has no
  terminal, so an `Ask` becomes a `Deny` rather than an assumption of consent.

- [The six risk levels](#the-six-risk-levels)
- [The three verdicts](#the-three-verdicts)
- [The order a decision is made in](#the-order-a-decision-is-made-in)
- [The built-in table](#the-built-in-table)
- [The layers](#the-layers)
- [Policy files](#policy-files)
- [The tightening-only rule, worked through](#the-tightening-only-rule-worked-through)
- [Force pushes are denied outright](#force-pushes-are-denied-outright)
- [Noninteractive execution](#noninteractive-execution)
- [External capabilities](#external-capabilities)
- [What proves this](#what-proves-this)

## The six risk levels

`RiskLevel` is the single definition of what executing a tool can affect. The
ordering lives in the type because policy compares against it, and a comparison
that means different things in different modules is not a policy.

| Level | Admits | Tools at this level |
| --- | --- | --- |
| `observe` | Reads state and changes nothing. | `fs.read`, `git.status`, `git.diff`, `workspace.inspect`, `workspace.search` |
| `workspace_write` | Writes inside the workspace, where the change is visible and revertible. | `fs.apply_patch` |
| `execute` | Runs a program, whose effects this process cannot enumerate in advance. | `process.exec`, `test.run`, `check.run` |
| `network` | Contacts a remote and may disclose local content to it. | — (v0.3 registers none) |
| `remote_write` | Changes state other people can see, which no local undo reaches. | — |
| `destructive` | Discards work that was not recorded anywhere else. | — |

They are *categories of consequence*, not a severity score: each admits
something the levels below it cannot do. Exactly one — `observe` — answers
`false` to `RiskLevel::mutates_state`, which is what makes "a read-only run" a
checkable claim rather than a convention.

A tool declares its level once, in its descriptor, and the registry never
rewrites it. **A tool cannot lower its declared risk for a particular call.** A
call that turns out to be *more* consequential than its level suggests — a path
leaving the workspace, a force variant selected in the input — is caught when the
invocation is classified, not by re-labelling the tool.

### The effective risk of one call

Three things raise the level a rule is selected against, and nothing lowers it:

```text
effective risk = max(
    the descriptor's declared risk,
    the risk of every contained path's access mode,   read → observe
                                                      write → workspace_write
                                                      destructive → destructive
    the risk of the non-filesystem flags,             executes → execute
                                                      network → network
                                                      remote write → remote_write
                                                      destructive → destructive
)
```

The result is a `RequestClassification`, which has private fields and no public
constructor: only `trust::classify_request` can produce one. `PolicyRequest`
carries that value rather than a risk level and a force flag, and floors it again
at the descriptor's risk. That is what stops a caller describing a request as
milder than the tool and its validated input make it.

## The three verdicts

`PolicyVerdict` is ordered `Allow < Ask < Deny`, so combining layers is `max`
and combining them safely is the only way the type supports.

| Verdict | Means |
| --- | --- |
| `allow` | The call may proceed with no further decision. |
| `ask` | A matching durable approval is required before execution. |
| `deny` | The call must not execute. |

Every decision also records *which layer bound it*, so an audit answers "who
decided this" without inference:

| `source` | The layer |
| --- | --- |
| `built_in` | Compiled-in safety defaults, or a hard built-in refusal. |
| `user_policy` | `<data_dir>/policy.json`. |
| `repository_policy` | `<workspace>/.harkness/policy.json`. |
| `run_grant` | A live approval the matcher already accepted for this exact candidate. |

A `run_grant` source may only ever accompany `allow` — the record refuses to
load otherwise — because a grant is an authorization and there is nothing else
for one to mean.

## The order a decision is made in

`PolicyEngine::evaluate` runs these in order, and the first that answers wins:

1. **External declaration checks.** A tool declaring more than one external
   capability, or declaring one without the context that names its subject, is a
   built-in `deny`. So is one whose observed identity evidence is missing.
2. **The force-push refusal.** Checked before any layer is read, so no rule and
   no grant can sidestep it.
3. **A policy file that could not be loaded.** A malformed, oversized, or
   future-versioned file is a `deny` naming the file, rather than a silent
   fallback to defaults.
4. **The layers**, combined with `max`: built-in, then user, then repository.
5. **A live matching grant**, which can only turn an `ask` into an `allow`.
6. **The noninteractive rule**, which turns a remaining `ask` into a `deny`.

Steps 4 and 5 are why a grant "answers only `Ask`": by the time grants are
consulted the verdict already exists, and `max` has already put a `deny` beyond
its reach.

## The built-in table

Trust is a positive decision, recorded once per workspace and bound to *both* a
project identity and a canonical root. An absent decision is untrusted.

| | `observe` | anything above `observe` |
| --- | --- | --- |
| **Trusted workspace** | `allow` | `ask` |
| **Untrusted workspace** | `ask` | `deny` |

Reading the table across rather than down is the point. Trusting a workspace does
not authorize anything; it moves the question from "may Harkness look at this at
all" to "may Harkness do this particular thing". **Every call above `observe`
asks, even in a trusted workspace.**

Recording the decision is explicit: `--trust-workspace` on a command that starts
a run, after reviewing the project root, or the equivalent in the window.
Without it a run is refused *before it is recorded at all*, because an untrusted
workspace denies everything above `observe` and asks about everything below it —
so the alternative is a run that is persisted and then immediately refused.

## The layers

Three layers combine with `max` on `Deny > Ask > Allow`:

```text
built-in default
   ∪ user policy        <data_dir>/policy.json
   ∪ repository policy  <workspace>/.harkness/policy.json
```

**`max` is also used inside one file.** A tool rule and a risk rule can both
match one request; the stricter wins rather than the more specific. A file that
denies a risk level has denied it, and a permissive rule for one tool in the same
file must not carve an exception out of it.

The reason a decision names is the selector that produced it — `risk execute`,
`tool fs.read`, `external capability push_remote_branch` — so a refusal says
which line of which file to look at.

## Policy files

Both layers share one strict, versioned document. The current version is `2`;
version `1` is still read, and a v1 file is upgraded only when something
explicitly rewrites it, so an old file cannot silently acquire a v2-only field
while still claiming to be v1.

| | Path | May contain |
| --- | --- | --- |
| User | `<data_dir>/policy.json` | `risks`, `tools`, `external_capabilities` |
| Repository | `<workspace>/.harkness/policy.json` | `risks`, `tools` |

`<data_dir>` is `HARKNESS_DATA_DIR` when set, else the platform data directory
(`~/.local/share/harkness` on Linux); `harkness --data-dir PATH` overrides both.

A complete user policy that never lets a process start without being asked, and
refuses one particular tool outright:

```json
{
  "version": 2,
  "risks": {
    "execute": "ask",
    "network": "ask",
    "remote_write": "deny",
    "destructive": "deny"
  },
  "tools": {
    "process.exec": "deny"
  }
}
```

A repository policy a project ships so that a checkout of it can be read but
never executed from:

```json
{
  "version": 2,
  "risks": {
    "execute": "deny"
  }
}
```

Four bounds apply to both files, and each exists because the file is data from
outside the process:

- **64 KiB maximum**, checked before the bytes are read into memory. Rules are a
  handful of short keys; anything larger is a mistake or an attempt to make a
  load allocate without bound.
- **Strict bodies.** An unknown field, an unknown verdict spelling, or a `tools`
  key that is not a valid tool identifier is a load failure.
- **The version is probed first.** A file from a newer build is an actionable
  upgrade error rather than something that looks corrupt.
- **A load failure is a `deny`, not a fallback.** Evaluation fails closed with a
  persisted, human-readable reason naming the file.

The repository file gets one bound the user file does not: it is resolved
*through the workspace boundary*. Its name is attacker-controlled even when its
bytes are reviewed, so committing `.harkness/policy.json` as a symlink out of the
worktree would otherwise make Harkness read — and apply — a file the workspace
does not contain. That resolves to a refusal, and the refusal is a load failure,
so evaluation fails closed rather than falling back to defaults.

## The tightening-only rule, worked through

A repository can raise a verdict and cannot lower one. The mechanism is just
`max`: a repository rule saying `allow` is combined with a built-in `ask` and
loses.

Take a `process.exec` call in a trusted workspace. The built-in table puts
`execute` at `ask`. Run it three ways, with everything else identical:

<!-- verified: exit=3 -->
```sh
# No repository policy: the built-in default asks, and a headless run refuses.
harkness --json tool invoke process.exec \
  --input '{"argv":["fixture-pass","--exact","scenario_process_fixture_pass_child","--ignored","--nocapture"],"timeout_seconds":30}' \
  --project ws
```

```text
kind: approval_required_noninteractive
message: noninteractive execution cannot answer an approval request
```

Now commit a repository policy that tries to widen it:

```json
{ "version": 2, "risks": { "execute": "allow" } }
```

The same command reports exactly the same thing:

```text
kind: approval_required_noninteractive
message: noninteractive execution cannot answer an approval request
```

The rule was read, matched, and combined — and `max(ask, allow)` is `ask`. The
repository did not weaken anything. Change one word so it tightens instead:

```json
{ "version": 2, "risks": { "execute": "deny" } }
```

```text
kind: policy_denied
message: deny by repository_policy rule for risk execute
```

`max(ask, deny)` is `deny`, and the reason names the layer and the selector.
A rule naming a tool behaves the same way and reports itself the same way:

```text
message: deny by repository_policy rule for tool fs.read
```

And a repository policy that cannot be parsed denies rather than being ignored:

```text
kind: policy_denied
message: denied: policy file …/ws/.harkness/policy.json is malformed:
         unknown variant `maybe`, expected one of `allow`, `ask`, `deny` at line 1 column 39
```

The user layer is symmetric in the other direction: it may tighten *or* widen
relative to the built-in table, because it is the user's own decision. A user
policy of `{"version": 2, "risks": {"observe": "ask"}}` makes even a file read
ask:

```text
kind: approval_required_noninteractive
message: noninteractive execution cannot answer an approval request
```

## Force pushes are denied outright

`--force` and `--force-with-lease` both overwrite history someone else may
already have fetched; the lease only narrows the window in which that happens.
v0.3 therefore refuses both, as a built-in `deny` checked *first* — before any
layer is read and before any grant is consulted:

```text
denied: force push is not permitted in v0.3 (force_with_lease)
```

The variant is named in the reason so the audit trail records which one was
asked for, and `ForcePush::force_pushing` marks the request as a remote write as
a side effect, so a caller cannot describe a force push as something less
consequential by omitting the remote-write flag.

## Noninteractive execution

An `Ask` that survives the layers and finds no matching grant needs a human. If
there is nobody to ask, the answer is `deny`:

```text
denied: noninteractive execution cannot answer approval;
        execution requires approval (built-in default for execute)
```

The decision keeps the `source` of the layer that produced the `ask`, so the
record still says which rule raised the question. On the command line this
surfaces as error kind `approval_required_noninteractive` at exit 3. Passing
`--interactive` asks on standard error and reads one line from standard input;
closing standard input is a denial, and Ctrl-C at a prompt cancels the run and
exits 130. See [Approvals](approvals.md#answering-one).

## External capabilities

v0.5's external subjects — an ACP agent, an MCP server and its tools, a forge,
a recipe — are controlled by somebody else and can change under a decision the
user already made. Policy treats them as ordinary requests with three
additions, and none of them is a way around anything above.

- **A normative risk floor per capability**, folded into the effective risk the
  same way a path's access mode is:

  | Capability | Floor |
  | --- | --- |
  | `launch_external_agent`, `connect_mcp_server`, `invoke_mcp_tool` | `execute` |
  | `read_forge_resource` | `network` |
  | `push_remote_branch`, `create_pull_request`, `modify_forge_resource` | `remote_write` |
  | `execute_workflow_recipe` | the maximum risk of its compiled steps |

- **A third rule map, `external_capabilities`,** in the *user* file only. A
  repository policy that names one with `allow` is refused at load: repository
  content may not grant external execution to the machine it is checked out on.
- **Typed denial kinds,** so a script can branch on
  `noninteractive_mcp_tool_invoke_denied` rather than on prose, and so a request
  whose identity evidence is missing is refused by name rather than evaluated
  against a subject nobody observed.

Trust in an external subject is a *precondition*, never an authorization: a
trusted agent still passes policy and approval on every action it takes.
`docs/agents.md` is the reference for the registry that decides which executables
may run at all.

## What proves this

| Claim | Package | Test |
| --- | --- | --- |
| The built-in table covers every trust/risk pair | `harkness-runtime` | `policy::tests::built_in_table_covers_every_risk_and_trust_branch` |
| A repository may tighten and may not weaken | `harkness-runtime` | `policy::tests::repository_policy_can_raise_and_cannot_lower_a_verdict` |
| No repository input at all can lower a verdict | `harkness-runtime` | `policy::tests::no_repository_policy_input_can_lower_any_verdict` |
| A tool rule never weakens a risk rule in one file | `harkness-runtime` | `policy::tests::a_tool_rule_never_weakens_a_risk_rule_in_the_same_file` |
| An understated classification raises no privilege | `harkness-runtime` | `policy::tests::an_understated_classification_cannot_lower_the_declared_risk` |
| Every force variant is a non-overridable denial | `harkness-runtime` | `policy::tests::every_force_variant_is_a_non_overridable_built_in_denial` |
| A noninteractive `ask` needs a live matching grant | `harkness-runtime` | `policy::tests::noninteractive_ask_requires_a_live_matching_grant` |
| No grant can answer a denial | `harkness-runtime` | `policy::tests::no_grant_can_answer_a_denial` |
| Malformed, unknown, and future files fail closed | `harkness-runtime` | `policy::tests::malformed_unknown_and_future_files_fail_closed_by_name` |
| A repository policy symlinked out of the workspace is refused | `harkness-runtime` | `policy::tests::a_repository_policy_symlinked_out_of_the_workspace_is_refused` |
| An oversized policy file is refused before it is parsed | `harkness-runtime` | `policy::tests::an_oversized_policy_file_is_refused_before_it_is_parsed` |
| Repository configuration cannot grant external execution | `harkness-runtime` | `policy::tests::repository_configuration_cannot_grant_external_execution` |
| The strict file form is frozen | `harkness-runtime` | `policy::tests::strict_versioned_file_round_trips_and_is_frozen` |
| Evaluation stays under its 5 ms budget | `harkness-runtime` | `policy::tests::policy_evaluation_meets_the_latency_target` |
