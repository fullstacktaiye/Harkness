//! The workflow-recipe boundary: turning a TOML file a user can edit into a plan
//! a run can be held to.
//!
//! A *recipe* is a declarative multi-step workflow — import a GitHub issue,
//! prepare a worktree, prompt an agent, run the tests, open a draft pull request
//! — written as TOML and kept beside the project or in a library. This crate
//! owns the source format and everything that happens before execution: the
//! probe-first versioned schema, the parser, validation, source and capability
//! analysis, and the compiler that produces a canonical execution plan. It owns
//! nothing about executing one. Step execution, approval gates, retry, cleanup,
//! cancellation, and resume are `harkness-runtime`'s ([#173], [#174]), which is
//! above this crate and which this crate may not name ([ADR-0009]).
//!
//! # Two artifacts, one of them durable
//!
//! The TOML source is **input**: user-editable, revisable, and never read again
//! once a run has started. The compiled canonical execution plan is the
//! **record**: pinned by content hash, persisted, and the only thing execution
//! and resume consume ([ADR-0015]). Editing a recipe mid-run therefore cannot
//! change what that run is doing — it changes what the *next* compile produces,
//! and a resumed run whose source has drifted says so instead of silently
//! becoming a different workflow.
//!
//! A recipe is executable content, so it is a trust subject: its source, its
//! capability set, and the identity of what it invokes are what a grant is
//! against, and a change to any of them invalidates the grant ([ADR-0016],
//! [#171]). A recipe cannot widen what Harkness will do — every step still passes
//! policy and approval on its own merits.
//!
//! # What is not here yet
//!
//! All of it. This crate is a compile-clean skeleton so that [#170] (schema,
//! parser, validation), [#171] (source, trust, and capability analysis), [#172]
//! (the compiler and dry-run preview), and [#175] (the built-in recipe and the
//! fixture library) each land against a decided contract instead of deciding one.
//!
//! [ADR-0009]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0009-v05-adapter-crate-boundaries.md
//! [ADR-0015]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0015-recipes-compile-to-persisted-plans.md
//! [ADR-0016]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0016-per-subject-trust-records.md
//! [#170]: https://github.com/fullstacktaiye/harkness/issues/170
//! [#171]: https://github.com/fullstacktaiye/harkness/issues/171
//! [#172]: https://github.com/fullstacktaiye/harkness/issues/172
//! [#173]: https://github.com/fullstacktaiye/harkness/issues/173
//! [#174]: https://github.com/fullstacktaiye/harkness/issues/174
//! [#175]: https://github.com/fullstacktaiye/harkness/issues/175

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
            "harkness-forge",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-recipe/Cargo.toml; ADR-0009 forbids an \
                 adapter crate from depending on anything above it or beside it",
            );
        }
    }
}
