# Registering an ACP agent

An ACP agent is a program somebody else wrote, launched as a child process on
your machine and spoken to over its standard input and output. Harkness never
downloads or installs one — that is an explicit non-goal — so using an agent
starts with telling Harkness about the copy you already have.

Four things have to be true before Harkness will run one, and it checks them in
this order every single time:

1. It is **registered** in `agents.json`.
2. It is **enabled**.
3. A **trust grant** covers it and currently stands.
4. The executable on disk still **hashes to what that grant was made about**.

Each of those is a separate, deliberate act. Discovery does not register.
Registering does not enable. Enabling requires trust. And trust is bound to
bytes rather than to a path, so replacing the program is something Harkness
notices rather than something it inherits.

All four gates live in `AgentRegistryService`, not in a user interface. A command
line or a window that forgot one of them cannot get past the service.

## `agents.json`

The registry lives beside `projects.json` in the Harkness data directory
(`HARKNESS_DATA_DIR`, or the platform data directory). It is small, diffable and
safe to edit by hand:

```json
{
  "schema_version": 1,
  "agents": [
    {
      "id": "gemini-cli",
      "display_name": "Gemini CLI",
      "command": "/usr/bin/gemini",
      "args": [],
      "env_allowlist": [
        "HOME",
        "PATH"
      ],
      "enabled": true,
      "source": "user"
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `id` | Your name for this registration. Lowercase ASCII letters, digits, `-`, `_` and `.`, beginning and ending with a letter or digit. |
| `display_name` | What a surface calls it. Never part of a trust decision. |
| `command` | **Absolute** path of the program. No `PATH` search happens at launch. |
| `args` | Arguments after the program name. A list, never a shell string. |
| `env_allowlist` | The *only* variables the agent process will see. |
| `enabled` | Whether Harkness may launch it. Absent means no. |
| `source` | `user`, `discovered`, or `development`. Provenance, not authority. |

`id`, `display_name` and `command` are required; everything else defaults to the
safe value, so a hand-written entry that says nothing about being enabled has
said "no".

The file follows the project catalog's rules exactly. `schema_version` is probed
before the body is parsed, so a file written by a newer Harkness produces an
*upgrade* message rather than a corruption message. An unknown field at a version
this build understands is refused rather than dropped, because dropping it would
lose data on the next write. Every write replaces the file atomically under the
stable `agents.lock` inode, and **no read ever rewrites it** — a read does not
even create the directory.

Nothing Harkness *observed* is in this file. The executable's digest, the
capability snapshot, the health record and the trust grant are all `runtime.db`
rows. Deleting that database costs you a re-trust and a re-check; it cannot cost
you a registration.

### Why `command` must be absolute

Because the trusted hash and the executed file must not be able to diverge. A
relative command is resolved against a search path at launch time, and a search
path is something an environment can change after you made your decision. The
absolute path is checked at registration, and the program at that path is hashed
again immediately before every launch.

### The environment allowlist

The agent starts from an **empty** environment and receives exactly the variables
you named that this Harkness process actually holds. There is no implicit
baseline — not even `PATH`. An agent that needs `PATH` says so, in a file you can
read.

That is stricter than the rule for a Harkness tool, deliberately: a tool is code
Harkness ships, and an agent is a program somebody else wrote running on your
workspace. "Which of my environment variables can it read" has one safe default,
and it is none of them.

Names are recorded exactly as you spell them. `path` and `PATH` are two different
variables on Unix, so Harkness will not fold one onto the other.

## Discovery

Discovery answers one question — "is a program with one of these names on the
search path" — and answers it with a path.

**It runs nothing.** No candidate is executed, opened, read, or hashed. A probe
that ran candidates "to check them" would turn enumeration into arbitrary code
execution, so the boundary is enforced in the service and asserted by a test that
points discovery at executables which record every invocation and observes zero.

A probe is bounded in time and in the number of search-path directories it looks
in, and it honours cancellation. When it stops early it says so —
`directory_budget`, `deadline`, or `cancelled` — because a truncated probe that
stayed quiet would read as "there is nothing else installed", which is the one
conclusion it cannot support.

What comes back is a suggestion. Registering it, trusting it, and enabling it are
three further things you do.

## Repository-provided suggestions

A checked-out repository may ship `.harkness/agents.json` in the same format. It
is read as a **suggestion** and never as configuration:

- every entry comes back disabled, whatever the file says;
- nothing is written to your own `agents.json`;
- a repository that asked for an agent to be enabled has that request *recorded*
  and not honoured, so a surface can tell you it asked;
- its `source` is replaced with `discovered`, so a repository cannot make an
  entry you never typed appear in a list as one you did.

Adopting one is a call you make, and it produces an ordinary disabled,
untrusted registration. Repository content may tighten Harkness's posture and may
never widen it.

## Trust

Trusting an agent computes the SHA-256 of the program at the configured path
*now* and binds the grant to that digest. Trust is never a boolean: the record
names the subject kind, the identity it was granted against, how far it reaches,
and when it was made.

Two things are deliberately **not** compared when the grant is checked:

- the display name, because trust never binds to a name;
- the executable's path, because an identical binary reached through another
  path is the same program, while a different binary at the same path is not.

A grant can be **global** or confined to **one workspace**. A workspace-scoped
grant does not reach outside the root it names — a launch from anywhere else is
*refused* with `agent_grant_out_of_scope`, and the grant is left exactly as it
was. Being used in the wrong place is not evidence that anything changed, so it
never costs you the grant.

A root is resolved before it is stored and before it is checked, so the
spellings that reach one checkout — a symlinked working copy, a relative path, a
path through `..` — are one workspace rather than several. A grant made in one
of them reaches a launch named in any of the others, and only a genuinely
different directory is out of scope.

Re-granting after drift re-affirms the *identity* and leaves the reach alone, so
asking for a different scope is a different decision and produces a new record
rather than quietly changing the old one.

### Re-trust after a change

When the bytes at the path stop matching the grant, Harkness:

1. marks the grant **invalidated**, recording the reason, the digest that was
   trusted, and the digest it found;
2. **disables** the registration, so the next launch is refused before it starts;
3. refuses the operation with `executable_hash_mismatch`, naming both digests.

Trusting it again continues the *same* record against the identity that is
actually there — the decision is the same one, re-affirmed. Revoking is different
and is terminal: trusting the agent after you said no is a *new* record, so the
refusal stays in the audit trail rather than being overwritten.

This means an agent that auto-updates asks you again on every release. That is
the accepted cost of the model: hashing cannot tell "the vendor shipped a patch"
from "someone replaced the binary", and being asked is better than not being
told.

### Trust is a precondition, never an authorization

A trusted agent still passes policy on every action, and still needs an approval
for anything the policy lattice says needs one. An agent's own permission system
supplements Harkness policy and never replaces it.

## Health checks

A health check spawns the agent, performs the ACP `initialize` exchange, and
tears it down under hard deadlines. It advertises **no** client capability at
all: each one is a promise to mediate a request the agent may then make, and a
health check mediates nothing.

The agent runs in a fresh, empty temporary directory rather than in a workspace —
it is being asked one question and needs no project to answer it.

The record is persisted whether or not the check succeeded, because an agent that
failed yesterday and has not been checked since is a different thing from an agent
nobody ever asked. That includes a command that is missing or is not a program:
nothing ran, but it is still something the check found out about the agent, and a
registry that said nothing had happened would be the least useful answer
available.

What is *not* recorded is the registry's own state — an agent that is unknown,
disabled, or untrusted, and one whose digest no longer matches its grant. The
last has its own durable consequence in the trust record and would only be
repeated here.

A record that carries a teardown rung is one where a program really did run,
which is how "your agent crashed on startup" stays distinguishable from "that
file is not a program".

| Status | Meaning |
| --- | --- |
| `healthy` | Answered `initialize`, wants no authentication. |
| `authentication_required` | Answered, and advertised authentication methods. Not a fault. |
| `incompatible` | Answered on a protocol version this build does not speak. |
| `failed` | Did not answer usefully. |

A successful check records the version the agent reports for itself, the
negotiated protocol version, and the capability snapshot, so a session start does
not have to re-negotiate to find out what the agent can do.

### Authentication is something you tell Harkness

ACP v1 has the agent authenticate itself, so an agent you are signed in to and
one you are not advertise exactly the same methods — nothing on the wire tells
them apart. A health check therefore records `required` for either, and launching
is refused until somebody says which it was. Completing the agent's own sign-in
and then recording it (`record_authentication`) is what clears it; a later health
check does not undo that, because still offering a way in is not asking again.

The record also keeps how far teardown had to go — `already_exited`,
`closed_stdin`, `signalled`, `killed`. "This agent had to be killed" is a bug
report about somebody's program rather than an implementation detail.

## Troubleshooting

Every failure carries a stable `kind()`. The ones you are most likely to see:

| Kind | What happened | What to do |
| --- | --- | --- |
| `unknown_agent` | Nothing is registered under that id. | Check the id, or register it. |
| `agent_already_registered` | The id is taken by a different configuration. | Update it instead of registering it. |
| `agent_disabled` | It is registered and switched off. | Trust it, then enable it. |
| `agent_not_trusted` | No grant covers it, or the one that did stopped applying. The message names what changed. | Trust it again after checking what changed. |
| `executable_hash_mismatch` | The program at the path is not the one that was trusted. Both digests are in the message. | Re-trust if you expected the change; investigate if you did not. |
| `executable_not_found` | Nothing is at the configured path. | Fix the path, or install the agent. The registration is kept. |
| `invalid_executable` | Something is there and cannot be run. The operating system's reason is in the message. | Check that it is a program and that it is executable. |
| `agent_authentication_required` | The agent advertised authentication and nobody has recorded completing it — or a recorded attempt failed. | Sign in through the agent's own flow, then record it. |
| `agent_grant_out_of_scope` | The grant is fine and says somewhere else. | Launch it in the workspace it was granted for, or trust it here too. |
| `agent_incompatible` | The last handshake selected a protocol version this build does not speak. | Use a build of the agent that speaks ACP v1. |
| `initialize_timeout` | It was launched, said nothing usable, and was terminated. | Check that the command really starts an ACP agent, and that any required arguments are in `args`. |
| `agents_file_version_too_new` | `agents.json` was written by a newer Harkness. | Upgrade Harkness. The file is untouched. |
| `agents_file_malformed` | The version is one this build reads and the body is not. | Fix the file; nothing partial was applied. |

Failures that belong to another layer keep that layer's spelling: an ACP refusal,
a transport disconnect, and a run-store failure each report the discriminant
their own namespace gave them rather than being re-spelled here.

## Multiple versions side by side

Two registrations pointing at two builds of one agent are two independent
subjects. They have their own identifiers, their own grants, their own health
records, and their own capability snapshots; revoking one leaves the other alone.
Uniqueness of the `id` is the only constraint, which is what makes keeping a
release build and a development build an ordinary thing to do rather than a
workaround.

## Related documents

- [`docs/acp.md`](acp.md) — what the handshake establishes and what each side
  advertises.
- [ADR-0016](adr/0016-per-subject-trust-records.md) — why trust is a per-subject
  record bound to an identity rather than a boolean.
- [ADR-0009](adr/0009-v05-adapter-crate-boundaries.md) — why the registry lives
  in `harkness-runtime` while the protocol lives in `harkness-acp`.
- [ADR-0012](adr/0012-stdio-only-protocol-transports.md) — how a peer process is
  launched, bounded, and torn down.
