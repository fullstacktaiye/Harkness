//! The forge boundary: issues, pull requests, and remote repositories, stated
//! in terms no hosting service owns.
//!
//! A *forge* is a code-hosting service with an issue tracker and a pull-request
//! model. This crate holds two things that have to stay separable: the
//! forge-neutral contracts a caller programs against ([#163]), and the GitHub
//! REST adapter that satisfies them ([#164]). The contracts are the point — a
//! second forge should be a new module in this crate rather than a change
//! anywhere above it — and the way the contracts stay honest is that nothing
//! upstream can reach the adapter directly.
//!
//! # The boundary
//!
//! GitHub wire types are **private to this crate**. Nothing in `harkness-runtime`
//! or above may name one, and no GitHub JSON shape is ever persisted; conversion
//! into forge-neutral domain types happens at this crate's public surface
//! ([ADR-0009]). Issue and pull-request bodies, titles, and comments are
//! attacker-controlled text under the same rule that governs repository content:
//! data, never instruction.
//!
//! Every request pins `X-GitHub-Api-Version: 2026-03-10`, and credentials arrive
//! as a `CredentialSource` reference — an environment variable or a file — never
//! a value this crate persists, logs, or hands to a prompt ([ADR-0018]).
//! Transport is blocking `ureq` on the calling worker thread, polling the same
//! `Cancellation` token the rest of the workspace already carries; no async
//! runtime enters through here ([ADR-0011]).
//!
//! A remote mutation — a push, a pull request, a comment — is where a mistake
//! becomes other people's problem. Mutations are approval-gated, idempotency-keyed,
//! and recoverable from an unknown completion ([#167], [#168]), and what Harkness
//! actually observed about one is recorded as such rather than inferred
//! ([ADR-0017]). A forge account and a forge repository are trust subjects, with
//! the host part of their identity rather than a subject of its own, so a
//! repointed remote invalidates the grant instead of inheriting it ([ADR-0016]).
//!
//! # What is not here yet
//!
//! All of it. This crate is a compile-clean skeleton so that [#163]
//! (forge-neutral contracts), [#164] (the REST adapter, rate limits, pagination,
//! typed errors), [#165] (local-to-remote repository mapping), [#166] (issue
//! listing and idempotent import), [#167] (approved push and remote-mutation
//! safety), [#168] (draft pull requests and unknown-completion recovery), and
//! [#169] (the fake forge and contract tests) each land against a decided
//! contract instead of deciding one.
//!
//! [ADR-0009]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0009-v05-adapter-crate-boundaries.md
//! [ADR-0011]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0011-blocking-http-for-the-forge-adapter.md
//! [ADR-0016]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0016-per-subject-trust-records.md
//! [ADR-0017]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0017-honest-observability-activity-classes.md
//! [ADR-0018]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0018-pinned-github-rest-api.md
//! [#163]: https://github.com/fullstacktaiye/harkness/issues/163
//! [#164]: https://github.com/fullstacktaiye/harkness/issues/164
//! [#165]: https://github.com/fullstacktaiye/harkness/issues/165
//! [#166]: https://github.com/fullstacktaiye/harkness/issues/166
//! [#167]: https://github.com/fullstacktaiye/harkness/issues/167
//! [#168]: https://github.com/fullstacktaiye/harkness/issues/168
//! [#169]: https://github.com/fullstacktaiye/harkness/issues/169

#![warn(missing_docs)]

#[cfg(test)]
mod tests {
    /// ADR-0009 draws two edges this crate may not have. It sits strictly below
    /// `harkness-runtime`, so it may not depend on the runtime or on a front end;
    /// and adapters do not depend on each other, so shared machinery goes below
    /// all four rather than sideways between two of them. A manifest is the only
    /// place either rule can be broken, so the manifest is what this reads — the
    /// sideways rule especially, since nothing else would catch it: no dependency
    /// cycle exists to trip on while the runtime does not yet name the adapters.
    /// The check is a plain substring search rather than a parse, which also
    /// catches a name in a `[dev-dependencies]` entry or in a comment claiming
    /// the rule no longer holds.
    #[test]
    fn the_manifest_names_no_crate_above_or_beside_this_one() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in [
            "harkness-runtime",
            "harkness-cli",
            "harkness-gui",
            "harkness-acp",
            "harkness-mcp",
            "harkness-recipe",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-forge/Cargo.toml; ADR-0009 forbids an \
                 adapter crate from depending on anything above it or beside it",
            );
        }
    }
}
