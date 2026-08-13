# Filesystem and process safety

Tool input paths are resolved by `harkness-runtime::trust::PathBoundary` before
use. The resolver canonicalizes the nearest existing ancestor, restores a
missing leaf, and checks the resulting path against the canonical workspace and
any explicitly granted extra roots. A symlink reached inside an allowed root
that resolves outside all allowed roots is refused. Tools receive a
`ContainedPath`, not the caller's unchecked path, so later process and
filesystem APIs can require evidence that containment already succeeded.

Non-Git child processes are described by `CommandSpec`: one executable, an
argv vector, a contained working directory, and an `AllowlistedEnv`. There is no
shell-command form. The child environment starts empty and copies only the
baseline `PATH`, `HOME`, `LANG`, `LC_ALL`, and `TERM` variables that exist in
the parent, plus exact validated names published by the tool descriptor.
Wildcard declarations are not supported. A concrete process request may
override only those same names; an input environment map cannot add a name the
descriptor did not publish. Baseline names may be overridden because they are
already part of every arbitrary-child contract.

`process.exec` and `test.run` build exclusively on this command shape and the
runtime's `ToolProcess` supervisor. Their child timeout defaults to 120 seconds
and is capped at 600; timeout and cancellation kill the process group rather
than only its leader. Standard output and standard error stream to artifacts,
with only bounded tails retained inline. `test.run` is the same supervisor with
an explicit command input and a pass/fail projection, not a second process
implementation or a command-discovery system.

The Git runner keeps its existing denylist model. Git is one known executable
whose credential helpers and askpass integration depend on the caller's
environment, so it inherits most variables while scrubbing values that can
redirect or inject Git behavior. Arbitrary tool children are unknown code and
therefore use the stricter allowlist. These are intentionally separate trust
models; tool process safety does not change Git behavior.

Workspace trust is a user decision bound to both `ProjectId` and the canonical
workspace root. No stored decision means untrusted. A moved checkout, a reused
path with a new project identity, or an unavailable root is also untrusted.
Trust means that executing the workspace's code may be considered by policy; it
does not make repository content authoritative instructions.
