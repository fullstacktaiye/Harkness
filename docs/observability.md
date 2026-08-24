# Diagnostics and redaction

Harkness records what it does so a run can be reconstructed afterwards, and
scrubs what it records so the reconstruction is not itself a credential leak.
Both live in `harkness-runtime`'s `observe` module, because they are the same
problem from either end: the more inspectable a run becomes, the more places a
secret can come to rest.

- [Diagnosing a run](#diagnosing-a-run)
- [Where the log lives, and what bounds it](#where-the-log-lives-and-what-bounds-it)
- [Controlling the log](#controlling-the-log)
- [What is redacted, channel by channel](#what-is-redacted-channel-by-channel)
- [The rules](#the-rules)
- [Declaring secrets, for tool authors](#declaring-secrets-for-tool-authors)
- [What redaction does not cover](#what-redaction-does-not-cover)

## Diagnosing a run

Every span the runtime opens carries `run_id` as a field of its own. That is the
whole indexing scheme: a run's diagnostics are whatever lines mention its
identifier, whichever thread produced them.

```sh
harkness --json run start …          # prints the run id in its result envelope
grep '<run-id>' ~/.local/share/harkness/logs/harkness.log | jq .
```

The same identifier is the key to everything else the run left behind, so a log
line and a timeline entry can always be put beside each other:

```sh
harkness --json run show <run-id>    # the recorded timeline
```

The fields are fixed and machine-parseable. Filter on them rather than on
message text, which may be reworded:

| Field | Appears on | Value |
| --- | --- | --- |
| `run_id` | every runtime span | the run's UUID |
| `step_id` | step and tool-call spans | the step's UUID |
| `tool_call_id` | tool-call spans | the call's UUID |
| `tool_id` | tool-call spans | the resolved tool identifier |
| `tool_version` | tool-call spans | the exact version that ran |
| `approval_id` | approval spans | the durable request's UUID |

Events carry their outcome as fields too — `decision`, `risk`, `outcome`,
`verdict`, `failure_kind` — so a question like "which calls in this run were sent
to a human" is one filter:

```sh
jq 'select(.span.run_id == "<run-id>" and .fields.decision == "ask")' \
  ~/.local/share/harkness/logs/harkness.log
```

The lines worth knowing about:

| Message | Level | Says |
| --- | --- | --- |
| `recovery sweep complete` | info | how many runs a crashed process left behind |
| `run started` / `run finished` | info | the run's own boundaries |
| `tool requested` | debug | the agent asked for a tool, before any gate |
| `policy decided` | info | `allow`, `ask` or `deny`, with the risk level |
| `waiting for an approval decision` | info | opens the span a human's wait is measured in |
| `approval decided` | info | which way it went |
| `tool call finished` / `tool call failed` | info / warn | the terminal outcome, with the failure detail |
| `cancellation requested` | info | somebody pressed stop |
| `the coordinator failed the run` | error | a fault outside any tool |

Two things are deliberately absent. There is no span inside the 20 ms
supervision poll or inside per-line output streaming, because the tool runtime's
budget is under 10 ms per call excluding the tool's own work. And the scheduler
emits nothing of its own: what it does is visible as the gap between
`tool requested` and `tool call finished`.

## Where the log lives, and what bounds it

```
<data_dir>/logs/harkness.log     the file being written
<data_dir>/logs/harkness.log.1   the previous one
…
<data_dir>/logs/harkness.log.4   the oldest kept
```

`<data_dir>` is `HARKNESS_DATA_DIR` when set, else the platform data directory
(`~/.local/share/harkness` on Linux); `harkness --data-dir PATH` overrides both.

- **Five files of 4 MiB each — 20 MiB, whatever happens.** Writing past the cap
  renames the generations down and deletes the oldest.
- **A line is never split across two files.** Rotation is decided before a line
  is written, not after, so the file stays parseable as JSON lines.
- **The directory is created by the first line, not at start-up.** A command that
  records nothing leaves a data directory it only read exactly as it found it.
- **`0700` on the directory, `0600` on the files** on Unix. Diagnostic lines quote
  process output and Git stderr; redaction is the first defence and the mode is
  the second.
- **A directory that cannot be written degrades to standard error** for the life
  of the process. Nothing here can fail the work it is describing.

## Controlling the log

| Variable | Effect |
| --- | --- |
| `HARKNESS_LOG` | `tracing` filter directives; defaults to `info`. e.g. `harkness_runtime=debug`, `harkness_runtime::coordinator=trace,info` |
| `HARKNESS_LOG_STDERR` | any value but empty or `0` mirrors every line to standard error |

`harkness --verbose <command>` is the flag form of `HARKNESS_LOG_STDERR`. The
mirror is the same JSON-lines rendering the file gets rather than a friendlier
one, for two reasons: what `--verbose` shows is then exactly what was recorded,
and `harkness --json`'s promise that standard error carries one JSON object per
line still holds.

A directive nobody can parse falls back to `info` rather than running without
diagnostics — a typo in an environment variable is not a reason to lose the log.

## What is redacted, channel by channel

Redaction happens **once, before persistence**, at the store boundary — not at
each of the many places a value is later shown. A front end, the CLI, the GUI and
the log therefore all read text that was already scrubbed, and adding a new
surface cannot add a new leak.

| Channel | Rules applied |
| --- | --- |
| run event payloads (string values; keys are field names) | all |
| approval summary and decision reason | all |
| tool result payloads (`tool_calls.output_json`) | all |
| failure messages (run, step, tool call) | all |
| task titles | all |
| artifact label and media type | all |
| artifact **content** | all but `private_key_block` |
| the diagnostic log | all |
| agent observations | all |
| tool **input** (`tool_calls.input_json`) | **none — see below** |
| `workspace_snapshots.payload_json` | **none — digest-bound** |

Redaction is idempotent: text that has already been through it is unchanged by a
second pass, so a value that crosses two boundaries is not marked twice.

## The rules

Each rule is named, and what it leaves behind names it: `«redacted:<rule>»`. The
marker never echoes any part of what it replaced.

| Rule | Matches | Example |
| --- | --- | --- |
| `declared_secret` | an exact value the process declared (see below) | any string |
| `url_userinfo` | a URL's whole userinfo | `https://user:hunter2@host` → `https://«redacted:url_userinfo»@host` |
| `authorization` | an `Authorization` header's value, or a bare `Bearer`/`Basic`/`token` credential | `Authorization: Bearer abc.123` |
| `credential_parameter` | a credential-shaped key and its value | `access_token=…`, `PGPASSWORD=…`, `"api_key": "…"` |
| `credential_token` | a credential whose issuer publishes its prefix | `ghp_…`, `github_pat_…`, `glpat-…`, `xox[bp]-…`, `AKIA…`, `AIza…`, `sk-ant-…`, `npm_…`, a JWT |
| `private_key_block` | a PEM private key, `BEGIN` line to `END` line | `-----BEGIN OPENSSH PRIVATE KEY-----…` |

The whole userinfo goes, not only the password half: `https://<token>@github.com`
is the commonest credential URL there is, and in
`https://x-access-token:ghp_…@host` the username names the scheme while the
password is the secret — keeping either half is a rule that leaks on the shape it
was most likely to meet. The host and path survive, which is what a run record
needs in order to say where a fetch went.

**There is no entropy scoring, deliberately.** A false positive here silently
rewrites the audit trail a user is relying on, in a way nothing downstream can
detect or undo. Every rule keys on a shape somebody published — which is also why
ordinary base64, commit hashes and UUIDs pass through untouched.

**No rule can reach across a quote or a bracket.** The diagnostic log is redacted
one already-encoded JSON line at a time, so a pattern that ran past a field
boundary would leave a record nothing could parse — which is worse than one it
failed to scrub. Every value class stops at `"`, `{`, `}`, `[` and `]`, and the
truncated-private-key case is bounded to the PEM alphabet for the same reason.

## Declaring secrets, for tool authors

A passphrase or an internal service token has no shape at all: nothing
distinguishes it from ordinary output. The one thing Harkness knows about such a
value is that it *handed it over* — so that is where it is declared.

**You usually do not have to do anything.** A tool that declares an environment
variable in its descriptor gets it copied into its child's environment by
`ToolProcess`, and any variable whose name looks like a credential has its value
declared automatically at the spawn, before the child can echo it anywhere:

```
ACCESSKEY  APIKEY  AUTHTOKEN  COOKIE  CREDENTIAL  PASSPHRASE
PASSWD     PASSWORD  PRIVATEKEY  SECRET  SESSIONTOKEN  TOKEN
```

matched case-insensitively against the name with `_` and `-` removed, so
`GITHUB_TOKEN`, `npm_config_authToken` and `MY_APP_API_KEY` are all covered.
`GIT_ASKPASS`, `SSH_ASKPASS` and `SSH_AUTH_SOCK` are explicitly *not*: they are
paths to a helper program or a socket, and scrubbing them would remove the field
that says which helper ran while protecting nothing.

To declare a value Harkness learned some other way:

```rust
use harkness_runtime::observe::{Declared, SecretRegistry};

match SecretRegistry::process().declare(&value) {
    Declared::Accepted | Declared::AlreadyKnown => {}
    Declared::TooShort => { /* under six bytes; see below */ }
}
```

The set is append-only and process-wide, and a redactor consults it on every
write — so declaring a value part-way through a run covers every record written
after that moment. Values are held in a buffer that overwrites itself before it
is freed, and there is no way to read one back out.

Two rules of thumb:

- **Never put a secret in a tool's input.** That column is not redacted, for the
  reason below.
- **A value under six bytes is refused.** Replacing every occurrence of a
  three-character string would corrupt far more records than it protects. Short
  secrets are covered by the shape rules or not at all.

## What redaction does not cover

Stated plainly, because a boundary nobody wrote down is a boundary somebody will
assume away.

- **A tool's input (`tool_calls.input_json`) is not redacted.** The executor reads
  those bytes back out of the column and *runs* them, and an approval's hash is
  taken over them — rewriting them would not protect a secret, it would run a
  different command than the one that was approved, against a record that no
  longer matches the decision made about it. Put secrets in a declared
  environment variable.
- **`workspace_snapshots.payload_json` is not redacted.** It is bound by a digest
  `harkness-context` re-derives on load, so rewriting a path inside it would move
  the digest and refuse the very row the rewrite was meant to protect. A snapshot
  holds hashes and paths, never file contents.
- **An artifact's content is filtered a line at a time, on bytes.** That keeps a
  binary artifact byte-identical and keeps memory bounded, and it costs one rule:
  `private_key_block`, the only rule that has to see across a newline, is not
  attempted on a stream. A line longer than 64 KiB is redacted and emitted in
  bounded chunks, so a credential straddling a chunk boundary is not seen.
- **The live CLI Git error path still prints raw `git` stderr to the terminal.**
  It predates this boundary and changing it would change a shipped contract. What
  is guaranteed is that such text is redacted when it is *persisted into a run
  record*.
- **A short secret, or one with no shape, that nobody declared, is not covered.**
  There is no mechanism that could find it without an entropy heuristic, and the
  cost of that heuristic is the audit trail.

## Adding a persisted channel

If you add a column, file, or stream that holds caller text, it owes the
redactor a pass, and it owes this document a row in the coverage table. The
`credential_redaction` integration test byte-scans the whole data directory after
a run that leaks through every channel it has; extend its fixture tool rather
than trusting a review to notice.
