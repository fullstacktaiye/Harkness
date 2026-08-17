# ADR-0020: Process liveness is an advisory lock, and a timestamp may only widen it

- **Status**: Accepted
- **Date**: 2026-08-17
- **Deciders**: Taiye Babatope
- **Implemented by**: [#98](https://github.com/fullstacktaiye/harkness/issues/98)
- **Builds on**: [#86](https://github.com/fullstacktaiye/harkness/issues/86) (run store), [#88](https://github.com/fullstacktaiye/harkness/issues/88) (append-only events), [#92](https://github.com/fullstacktaiye/harkness/issues/92) (durable approvals), [#97](https://github.com/fullstacktaiye/harkness/issues/97) (run coordinator)

## Context

A Harkness process that dies mid-run leaves its records exactly as they were:
the run `running`, its call `awaiting_approval`, its question `pending`, and no
worker anywhere to move any of them. Nothing sweeps them, so they stay that way
for good — indistinguishable from live work. A front end shows progress nothing
is behind, a retry is refused because the run "is still active", and the run
history that [#82](https://github.com/fullstacktaiye/harkness/issues/82) and
[#83](https://github.com/fullstacktaiye/harkness/issues/83) are built on quietly
stops describing anything.

Detecting this needs an answer to one question — *is the process that claimed
this run still alive* — and the answer has to survive the case that matters,
which is a process that got no chance to say anything. `SIGKILL`, an abort
inside the runtime, and power loss all write nothing.

Three signals were available, and they disagree in exactly the interesting case.

**`runs.owner_pid`** has existed since migration 1 and cannot answer it. Process
identifiers are reused, so a row naming a live pid is not evidence that the
process holding it is the one that wrote the row. Checking whether a pid exists
would eventually mark a live process's runs abandoned because an unrelated
program inherited its number.

**A heartbeat timestamp** is the obvious answer and is what most schedulers use.
It is also the one signal that is wrong in the direction that costs something:
a process can be alive and not renewing. A build that stalls on a network mount,
a debugger stopped at a breakpoint, and a machine that suspended for an hour all
stop refreshing a column while their worktrees stay held. Ending those runs
writes a false ending into the history of work that is, in every sense a
filesystem cares about, still in flight — and if the stalled process resumes, two
attempts are mutating one checkout.

**An OS advisory lock** is released by the kernel when its holder dies, however
it died and whether or not it cooperated. `harkness-git`'s repository lock and
`harkness-core`'s `ManagedImportLock` already rely on exactly that property, and
`README.md` already documents the recovery it makes possible (`project reconcile`
removing managed directories a killed clone left behind). A lock that can be
taken is proof the holder is gone. What it cannot do is notice a process that is
alive and stuck.

[#98](https://github.com/fullstacktaiye/harkness/issues/98) asked for both — a
lock probe *or* a stale timestamp, "so a hung-but-alive process is eventually
treated as dead" — while also requiring, under security, that the logic "never
[fail] toward claiming a live process's runs" and that "timestamps alone only
widen, never shorten, the survival window". Those two cannot both hold: a hung
process is a live process. This ADR is the record of which one wins.

## Decision

**A lock file is the death signal. A timestamp may only ever widen the window in
which a claim is treated as alive.**

Each coordinator takes one lease at construction: a `LeaseId`, an advisory lock
on `<data_dir>/locks/runtime-lease-<id>.lock` held for the coordinator's life,
and a `runtime_leases` row that every run it starts points at through
`runs.lease_id`. The file is the liveness oracle and the row is the durable
record; neither alone would do, because a row cannot notice a `SIGKILL` and a
lock file names nothing.

`interruption_reason` decides deadness from four answers, in this order: a run
with no claim at all, a claim its holder released, a lock the kernel has given
back, and — only when the lock cannot be probed at all — a `renewed_at` older
than `LEASE_EXPIRY_GRACE`. **A held lock outranks every timestamp.** A wedged
coordinator keeps its runs, deliberately, for the whole time it is wedged.

Ordering is load-bearing in two places. The lock is taken *before* the row
exists, so a row whose lock is not yet held is not a state the store can be found
in; and the row is written with the first run that claims it, so a read-only
front end that opens a store and records nothing leaves nothing to collect. A
lease identity is never reused, which is what makes removing a proved-dead lock
file safe: no later process will ever open that path.

**The sweep claims only what it can prove, once, before any work is accepted.**
It runs at coordinator construction under a short-lived exclusive
`runtime-recovery.lock`, reads state spellings and lease identities rather than
timelines, and takes one transaction per run so a single poisoned record cannot
block the recovery of the rest. A claim is read and probed once however many runs
name it, so one death is described one way. A live sibling process — the command
line beside a running application — is untouched.

**Recovery only appends, and `interrupted` has exactly one author.** The run,
its unfinished steps, its in-flight calls and its pending approvals each reach a
terminal state with their own appended event beside a `run_interrupted` entry
naming what was detected; nothing already recorded is deleted or rewritten. A
pending question becomes `Superseded`, which [#92](https://github.com/fullstacktaiye/harkness/issues/92)
already defined as "the run will not resume, so the question no longer has a
subject" and made terminal. No code inside a live process may write
`ExecutionState::Interrupted` for a run: a coordinator that marked its own run
interrupted because one call ended without a verdict would be recording that the
owning process had stopped while demonstrably still running. Such a call becomes
a `ToolFailed` observation carrying the `interrupted` kind, and the agent decides.

**Recovery ends a run; it never resumes one.** `retry_run` creates a *new* run
for the same task, recording `retry_of` on it and one `run_retried` line on the
original, whose state and history are otherwise untouched. No grant carries over,
because grants are matched on the run they were given for and the retry has a new
identity. `workspace_may_be_modified` is set whenever the earlier attempt started
a call that could write — read from `started_at`, which the transition into
`running` sets and nothing else does — and a tool this build no longer registers
counts as one that could write.

## Consequences

A killed process no longer leaves runs that read as live for ever, and the
records it left stay inspectable: the timeline up to the moment it stopped is
exactly what that process wrote, with the ending appended after it.

Two Harkness processes can share a data directory safely, which they already
could not be stopped from doing. The proof is per-claim and local, so the cost of
being wrong is bounded by what a lock can say.

The accepted cost is stated plainly: **a hung-but-alive coordinator keeps its
runs indefinitely.** There is no timeout that ends them, and a user whose
Harkness is wedged sees runs that stay `running` until that process is killed —
at which point the next start ends them. This is the deliberate trade against
claiming a live process's workspace, and reversing it means superseding this ADR
rather than relaxing a constant.

Detection happens at startup and nowhere else. A process that dies while another
Harkness is already running leaves runs that stay non-terminal until *some*
coordinator is constructed against that data directory. No background daemon
watches for crashes, and adding one is a separate decision.

Contributors gain three obligations. Anything that writes
`ExecutionState::Interrupted` for a run belongs in the sweep. Any new liveness
input may only widen the survival window, never shorten it. And a lease or
recovery lock must never be held while the scheduler slot, the repository lock,
or the catalog lock is acquired, and never while a store transaction is open.

## Alternatives considered

**Heartbeat timestamps alone, with a grace period.** The conventional design, and
the one the issue's technical-design section asked for. It ends the runs of any
process that stops renewing, which includes every process that is alive and
stalled — a suspended laptop, a stopped debugger, a blocked network mount. The
failure is silent and the damage is not recorded anywhere: the run gets a
plausible-looking `interrupted` ending while its worktree is still held, and a
retry offered on the strength of it runs a second attempt against a checkout the
first is still mutating. A signal that is wrong about live processes is worse
here than a signal that is silent about stuck ones.

**Lock probe *or* stale timestamp, as the issue's design bullet specified.** This
is the heartbeat design with a cheaper common case, and it inherits the same
failure exactly: the `or` fires precisely when the lock says "held" and the clock
says "stale", which is the hung-but-alive process. The issue's own security
section forbids the outcome its design section requests, and its definition of
done requires that "live sibling processes are never disturbed". A hung sibling
is a live sibling.

**Probe the recorded `owner_pid`.** No new table, no new file, and it reuses a
column that already exists. Process identifiers are reused, so it answers "some
process has this number", which is not the question. It is also the least
portable of the three, and it would end a live process's runs for the most
arbitrary reason available.

**Mark runs `interrupted` on clean shutdown instead of relying on the sweep.** A
process that exits politely could write the ending itself, leaving recovery to
handle only crashes. It halves the mechanism and keeps none of the hard half: the
sweep still has to exist for `SIGKILL`, and a shutdown path that writes terminal
states races the workers still finishing their own. Releasing the claim and
letting the next start decide keeps one code path for both endings, and a run
that really did finish during shutdown records its own outcome first.

**Resume interrupted runs from their last step.** The outcome a user actually
wants, and the one v0.3 cannot honestly offer. A step's effect on the workspace
is not recorded in enough detail to know what re-running it would repeat, and
nothing rolls back a partial mutation. A fresh run with an honest
`workspace_may_be_modified` flag says what is true; checkpoint-resume waits for a
context model that can say more.
