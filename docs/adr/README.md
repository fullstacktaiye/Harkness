# Architecture Decision Records

An ADR records one decision that is expensive to reverse, together with the
reasoning that made it the right one. It exists so a future contributor — or the
author six months later — can evaluate a proposal against a written decision
instead of re-litigating it in a review thread.

Write an ADR when a choice fixes a crate boundary, a dependency direction, a
persisted format, a trust boundary, or a concurrency model. Do not write one for
a decision a reader can recover from the code: the code is the authority on what
Harkness does, and an ADR is the authority on why it may not do otherwise.

`AGENTS.md` remains the normative statement of repository conventions and
durable-format invariants. An ADR explains a decision; `AGENTS.md` states the
rule a contributor can violate silently. Where the two overlap, `AGENTS.md`
wins as the instruction and the ADR supplies the rationale.

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-v04-crate-boundaries.md) | v0.4 crate boundaries | Accepted |
| [0002](0002-provider-agent-taxonomy.md) | Model provider, native agent, and external coding agent are three contracts | Accepted |
| [0003](0003-blocking-http-and-sse.md) | Blocking HTTP with SSE on the calling worker thread | Accepted |
| [0004](0004-evidence-versus-index-cache.md) | Run evidence is durable; the context index is a disposable cache | Accepted |
| [0005](0005-deterministic-retrieval-first.md) | Deterministic retrieval is the foundation; embeddings are optional | Accepted |
| [0006](0006-repository-content-is-untrusted.md) | Repository content is untrusted data | Accepted |
| [0007](0007-openai-compatible-tracer-bullet.md) | One OpenAI-compatible adapter as the tracer bullet | Accepted |
| [0008](0008-workspace-snapshot-identity.md) | Workspace identity is a composite digest, never `HEAD` alone | Accepted |
| [0009](0009-v05-adapter-crate-boundaries.md) | v0.5 adapter crate boundaries and wire-type privacy | Accepted |
| [0010](0010-official-acp-schema-crate.md) | Adopt the official ACP schema crate rather than hand-rolled wire types | Accepted |
| [0011](0011-blocking-http-for-the-forge-adapter.md) | Blocking `ureq` on worker threads for the forge adapter | Accepted |
| [0012](0012-stdio-only-protocol-transports.md) | stdio-only protocol transports behind a transport seam | Accepted |
| [0013](0013-mcp-stateless-with-legacy-fallback.md) | MCP 2026-07-28 stateless is primary; 2025-11-25 is a probe-selected fallback | Accepted |
| [0014](0014-acp-protocol-version-one.md) | ACP protocol version 1 only; v2 is a negotiation boundary | Accepted |
| [0015](0015-recipes-compile-to-persisted-plans.md) | A recipe is source; the compiled plan is the record | Accepted |
| [0016](0016-per-subject-trust-records.md) | Trust is a per-subject record bound to an identity, never a boolean | Accepted |
| [0017](0017-honest-observability-activity-classes.md) | Every activity carries an evidence class, and a claim is never shown as a fact | Accepted |
| [0018](0018-pinned-github-rest-api.md) | Pin `X-GitHub-Api-Version: 2026-03-10`, authenticate with a fine-grained PAT | Accepted |

This index is the ordering authority. If two ADRs conflict, the conflict is
resolved before either merges; a lower number does not automatically win.

ADR-0001 through ADR-0008 are the v0.4 set from
[#108](https://github.com/fullstacktaiye/harkness/issues/108), which established
this directory, the template below, and the numbering. ADR-0009 through ADR-0018
are the v0.5 set from
[#145](https://github.com/fullstacktaiye/harkness/issues/145). The two issues were
written to proceed in parallel, with whichever merged first fixing the
conventions and the other adopting them; [#108](https://github.com/fullstacktaiye/harkness/issues/108)
merged first, so [#145](https://github.com/fullstacktaiye/harkness/issues/145)
continues its numbering rather than restarting it. Milestone grouping is a fact
about how these records happened to be written, not a structure — a later ADR
supersedes an earlier one across milestone lines like any other.

## Status vocabulary

- **Proposed** — written, under discussion, not binding. Code may not rely on it.
- **Accepted** — binding. New code must conform; a change that contradicts it is
  a review-blocking defect until the ADR is superseded.
- **Superseded by ADR-NNNN** — the decision no longer holds. The record stays as
  written, with only the status line and a pointer added.

An accepted ADR is never edited to reflect a change of mind. Amend it by
writing a new ADR that supersedes it, so the history of the decision survives.
Correcting a typo, a dead link, or a stale issue number is not a change of mind
and needs no supersession.

## Numbering and file names

Numbers are allocated in order, zero-padded to four digits, and never reused.
The file name is `NNNN-kebab-case-title.md`. A number is claimed by the PR that
merges the record, so two open branches must not both claim the same one; if
they do, the second to merge renumbers.

## Template

```markdown
# ADR-NNNN: Title

- **Status**: Proposed
- **Date**: YYYY-MM-DD
- **Deciders**: names
- **Implemented by**: #issue, #issue
- **Builds on**: #issue, ADR-NNNN

## Context

What is true today, what is about to change, and what forces make the decision
necessary. Cite real files and line numbers, and mark planned contracts as
planned. This section states facts, not preferences.

## Decision

The decision, in the imperative, with its boundaries drawn precisely enough that
a reviewer can tell conforming code from non-conforming code. Name what is
forbidden, not only what is chosen.

## Consequences

What this costs, what it enables, and what a contributor now has to do that they
otherwise would not. Include the consequences that are inconvenient — an ADR
that lists only benefits records an advertisement, not a decision.

## Alternatives considered

Each alternative that was genuinely on the table, and the specific reason it
lost. "It was worse" is not a reason.
```
