# ADR-0015: A recipe is source; the compiled plan is the record

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#170](https://github.com/fullstacktaiye/harkness/issues/170), [#171](https://github.com/fullstacktaiye/harkness/issues/171), [#172](https://github.com/fullstacktaiye/harkness/issues/172), [#173](https://github.com/fullstacktaiye/harkness/issues/173), [#174](https://github.com/fullstacktaiye/harkness/issues/174)
- **Builds on**: ADR-0009, ADR-0016, ADR-0006 (repository content is untrusted), [#92](https://github.com/fullstacktaiye/harkness/issues/92) (durable approvals)

## Context

A workflow recipe is a declarative multi-step workflow — import a GitHub issue,
prepare a worktree, prompt an agent, run the tests, open a draft pull request —
written as TOML and kept beside a project or in a library. It is a file on disk
that a user can open in an editor at any moment, including the moment a run based
on it is executing.

Runs in Harkness are long, interruptible, and resumable. A recipe run can span
minutes, wait on a human approval, survive an application restart, and resume
afterwards. That combination makes the source file's mutability a correctness
problem rather than a stylistic one. Three concrete failures follow from
re-reading TOML at each step:

- A user edits the recipe while a run is waiting on an approval. Execution
  resumes into steps the user approved nothing for.
- A run resumes after a restart from a file that has changed, and silently
  becomes a different workflow with the same run identifier.
- The audit trail cannot answer what a completed run actually did, because the
  only record is a file whose current contents are not evidence of its past
  contents.

The repository already answers the general form of this question consistently.
Approvals bind to an exact request ([#92](https://github.com/fullstacktaiye/harkness/issues/92)).
Tool descriptors publish schemas generated from their types so a contract cannot
disagree with the code. Workspace identity is a composite digest rather than a
mutable pointer (ADR-0008). Persisted records carry explicit schema versions with
frozen fixtures. In each case, the durable thing is a snapshot, not a reference to
something that can move.

## Decision

**A recipe has two artifacts, and only one of them is durable.**

**The TOML source is input.** It is user-editable, revisable, versioned by a
probe-first `schema_version` like every other Harkness format, and read exactly
once per run: at compile time.

**The compiled canonical execution plan is the record.** It is deterministic,
pinned by content hash, persisted *before the run starts*, and it is **the only
thing execution and resume consume**. Nothing in the execution path re-reads the
source file. Not on resume, not on retry, not to look up one field.

The plan is canonical in the sense that matters for hashing: compiling the same
recipe with the same inputs and the same resolved environment twice produces
byte-identical output. It carries `PLAN_SCHEMA_VERSION`, the recipe identity and
its source content hash, the resolved dependency graph, pinned tool versions,
the agent installation identity (executable hash, or `native`), MCP tool schema
fingerprints, resolved inputs, the capability manifest, approval gates, budgets,
and cleanup steps
([#172](https://github.com/fullstacktaiye/harkness/issues/172)).

**Everything the plan depends on is pinned into it, not referenced.** A tool
version, an agent's executable hash, an MCP schema fingerprint — the plan records
the value observed at compile time, so drift is detectable rather than absorbed.

**Compilation is where refusal happens.** Untrusted source, recipe hash drift, an
unknown tool version, a missing agent, a changed MCP schema fingerprint, or an
unresolved input are compile-time refusals with typed, machine-readable errors.
A run that has started has already passed those checks.

**Resume compares, it does not re-parse.** A resumed run loads its persisted plan
and continues. If the source file has drifted, that is reported as drift — the
run is still executing the plan it was compiled from — and the user decides
whether to recompile, which produces a *new* plan with a new hash
([#174](https://github.com/fullstacktaiye/harkness/issues/174)).

**Dry-run renders the plan and performs no side effects.** It reads the registry
and trust records and nothing else: no ACP or MCP connection, no `git push`, no
forge `POST`. A preview that can create a pull request is not a preview.

**Secrets are never materialized into the plan.** Credential references stay
references, under the same `CredentialSource` discipline ADR-0018 applies to forge
tokens. The plan is persisted and inspectable; anything in it is disclosed.

**Compiling is not authorizing.** A plan describes what will be attempted; every
step still passes policy and approval on its own merits at execution time. Under
ADR-0006 a repository-provided recipe is untrusted content, and it can no more
widen what Harkness will do than an `AGENTS.md` can.

## Consequences

- Editing a recipe mid-run is safe and does nothing surprising: it changes what
  the next compile produces. This is the single behavior most of the decision is
  purchased for.
- A completed run has a durable, hash-identified record of exactly what it was
  going to do, which is what makes an approval auditable after the fact and what
  makes [#181](https://github.com/fullstacktaiye/harkness/issues/181)'s flagship
  end-to-end assertion checkable.
- Users who expect "fix the typo and hit resume" will be surprised. Resume
  continues the compiled plan and reports the drift; fixing a recipe means
  recompiling, which is a new plan and, where capabilities changed, new
  approvals. The extra step is the guarantee.
- Plans are persisted for every run and they are not small — a dependency graph
  plus pinned identities and a capability manifest. Storage grows with run
  history, and plan retention becomes a real question the way event and artifact
  retention already are.
- Determinism is a requirement on the compiler, not a hope. Anything that varies
  between two compiles of the same input — map iteration order, a timestamp, an
  absolute path that differs per machine — is a bug, because the content hash is
  load-bearing.
- Two schema versions now need managing, the recipe's and the plan's, each with
  frozen fixtures. A plan whose `PLAN_SCHEMA_VERSION` is newer than the running
  binary understands is inspect-only rather than executable, which is the same
  posture the catalog and runtime formats already take on a downgrade.
- Compile-time pinning means a plan can be refused for a reason that has nothing
  to do with the recipe — an agent updated itself, an MCP server changed a
  schema. The error has to name which pin drifted, or the refusal is unactionable.

## Alternatives considered

**Interpret the TOML directly at each step.** The obvious implementation, and the
one with no second artifact to version, hash, or store. Rejected: it is exactly
the mid-run-edit failure, and it makes resume unsound in a way no amount of
careful coding fixes. It also leaves nothing to audit — the record of what a run
did would be a file that has since changed.

**Parse once at run start and hold the plan in memory.** Fixes mid-run edits for
a process that stays alive, costs nothing, adds no format. Rejected: it does not
survive a restart, and surviving a restart is the whole point of durable runs. A
resumed run would have to re-parse, which is the rejected alternative above with
extra steps and a narrower window.

**Hash the source file and refuse to resume if it changed.** Much simpler: one
hash, no compiled artifact, and mid-run edits are caught. Rejected: it turns
every recipe edit into a dead run, and it still cannot answer what the run was
doing — a hash proves the source is unchanged without recording what it said.
Refusing to resume is also the wrong default when the user's edit was to a
later step, or a comment.

**Store the raw TOML text as the durable record**, parsing it on resume. Solves
mid-run edits and auditability with no compiler. Rejected: parsing is not the
expensive or risky part — *resolution* is. Tool versions, agent identity, MCP
fingerprints, and inputs are resolved against a world that moves, and text
carries none of that. Two resumes of the same stored text could still do
different things.

**Compile lazily, one step at a time, so the plan reflects the current world.**
More adaptive, and a step could pick up a tool version fixed since the run
started. Rejected: it makes the capability manifest unknowable in advance, which
makes approval gates and dry-run preview impossible — a user cannot approve what
has not been decided. Adaptivity is the property being traded away on purpose.
