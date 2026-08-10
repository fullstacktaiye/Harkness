## Summary

<!-- What changed, and why is it needed? Keep this focused on user or maintainer impact. -->

## Scope

<!-- Check the areas affected by this PR. Leave unrelated boxes unchecked. -->

- [ ] Core/catalog/project storage
- [ ] Git operations, process execution, locking, or worktrees
- [ ] Runtime task/run/step/tool-call records
- [ ] CLI behavior or JSON contract
- [ ] GUI, Rust/CXX-Qt bindings, or QML
- [ ] Documentation, packaging, or CI

Related issue(s):

## Behavior and design

### User-visible behavior

<!-- Describe the behavior before and after, including error or cancellation paths. -->

### Review focus

<!-- Call out non-obvious design choices, trade-offs, or areas where reviewers should
concentrate. Mention anything intentionally deferred. -->

## Compatibility and safety

- [ ] This change does not alter a durable file format, persisted state, or CLI wire contract.
- [ ] If the project catalog format changed, I updated the schema/version handling, compatibility behavior, and frozen catalog fixtures; read-only operations still do not rewrite it.
- [ ] If a runtime record format or persisted state spelling changed, I bumped its schema version and updated the frozen fixtures and strict deserialization tests.
- [ ] If the CLI contract changed, I updated the contract output, documentation, and relevant integration tests.
- [ ] If Git, filesystem, or process behavior changed, I considered locking, cancellation, path safety, credential handling, and failure cleanup.
- [ ] If worktree behavior changed, parent/child validity, Git administrative state, and concurrent catalog/repository access remain safe.
- [ ] If this adds or changes a destructive action, it remains explicit, guarded, and covered by refusal-path tests.
- [ ] Not applicable (explain below).

Compatibility or safety notes:

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `RUSTDOCFLAGS=-D warnings cargo doc --locked -p harkness-runtime --no-deps`
- [ ] GUI/QML smoke coverage was run when applicable (Qt 6 and `qmake` available).
- [ ] The CMake release build was run when packaging or installation behavior changed.
- [ ] The opt-in network integration tests were run when remote Git behavior changed.
- [ ] Not run (explain why):

## UI evidence

<!-- For visible GUI/QML changes, add before/after screenshots or a short recording.
Delete this section when not applicable. -->

## Known limitations or follow-up

<!-- Include migration, platform, Qt/KDE, network, or rollout caveats. Write "None" if clear. -->

