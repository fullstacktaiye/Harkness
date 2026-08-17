//! The boundary every model endpoint reaches Harkness through.
//!
//! Three things in this ecosystem are called an agent, and they are three
//! different contracts. ADR-0002 fixes the vocabulary; this crate is where one
//! of the three lives, and the rustdoc says which so nobody has to guess:
//!
//! | Term | What it is | Where it lives |
//! | --- | --- | --- |
//! | **model provider** | An endpoint that accepts messages and tool definitions and streams back text and tool-call *requests*. It executes nothing. | [`contract::ModelProvider`], here |
//! | **native agent** | Harkness itself: planning, context, prompts, tool execution, policy, approvals, persistence, retry, completion. | `harkness_runtime::agent::Agent` |
//! | **external coding agent** | A separate program that owns its own loop and edits files itself, asking Harkness for permission. | the ACP milestone, [#149]–[#156] |
//!
//! No type implements two of them, and no public item in this workspace is
//! named `AgentProvider`. The three meet in one place and in one direction: the
//! native agent *consumes* a model provider between its own decisions.
//!
//! # What this crate is for
//!
//! Everything provider-specific stops here. An adapter parses its endpoint's
//! wire format into [`contract`] types and keeps its own structs private, so
//! `harkness-runtime`, the run store, and QML never learn what shape an
//! endpoint's JSON has — which is what makes a second adapter a new
//! implementation of one trait rather than a rewrite of everything above it.
//!
//! - [`contract`] — the provider-neutral vocabulary: identities, capabilities,
//!   messages, the streamed event model, one turn's outcome, and ten stable
//!   error kinds.
//! - [`assemble`] — the streaming assembler that turns raw events into a
//!   validated [`AssistantTurn`](assemble::AssistantTurn), including the calls
//!   it had to mark invalid rather than drop.
//! - [`scripted`] — a deterministic provider that replays frozen JSON scripts,
//!   so the whole surface above is exercised with no network, no credential,
//!   and no paid API.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use harkness_provider::{
//!     Cancellation,
//!     contract::{
//!         ModelId, ModelMessage, ModelProvider, ModelRequest, Role, SinkControl, StopReason,
//!     },
//!     scripted::ScriptedProvider,
//! };
//!
//! let provider = ScriptedProvider::scenario("single_tool_call")?;
//! let request = ModelRequest::new(
//!     ModelId::new("scripted-model")?,
//!     vec![ModelMessage::text(Role::User, "Read src/lib.rs, please.")],
//! );
//!
//! // The sink sees the turn as it arrives; the outcome is it, assembled.
//! let mut streamed = 0;
//! let mut sink = |_event| {
//!     streamed += 1;
//!     SinkControl::Continue
//! };
//! let outcome = provider.stream(&request, &mut sink, &Cancellation::default())?;
//!
//! assert_eq!(outcome.stop, StopReason::ToolUse);
//! assert_eq!(outcome.turn.tool_calls[0].name(), Some("fs.read"));
//! assert_eq!(streamed, outcome.event_count as usize);
//! # Ok(())
//! # }
//! ```
//!
//! # What this crate is not for
//!
//! No HTTP, no SSE, no endpoint or credential types ([#124], [#125]); no prompt
//! construction or context budgeting ([#127], [#122]); no agent loop, retry
//! orchestration, or persistence ([#126], [#128]); and no token estimation
//! beyond passing through what a provider reported ([#122]).
//!
//! It also persists nothing. The script fixtures carry a `{"v": 1}` probe so a
//! future format asks for an upgrade rather than reading as corrupt, and the
//! records here become `runtime.db` columns only when [#126] writes them.
//!
//! # Trust
//!
//! **Nothing a provider streams back is an instruction to Harkness.** Model
//! output is content: it can ask for a tool call and the runtime decides
//! whether that call happens, under the same policy and approval gates a user's
//! own request goes through. Repository content and tool output travelling the
//! other way are untrusted in the same sense (ADR-0006), and delimiting them so
//! a model cannot mistake one for the system's voice is [#127]'s job.
//!
//! [#122]: https://github.com/fullstacktaiye/harkness/issues/122
//! [#124]: https://github.com/fullstacktaiye/harkness/issues/124
//! [#125]: https://github.com/fullstacktaiye/harkness/issues/125
//! [#126]: https://github.com/fullstacktaiye/harkness/issues/126
//! [#127]: https://github.com/fullstacktaiye/harkness/issues/127
//! [#128]: https://github.com/fullstacktaiye/harkness/issues/128
//! [#149]: https://github.com/fullstacktaiye/harkness/issues/149
//! [#156]: https://github.com/fullstacktaiye/harkness/issues/156

#![warn(missing_docs)]

pub mod assemble;
pub mod contract;
pub mod scripted;

mod text;

/// The workspace's cancellation token, re-exported.
///
/// Re-exported rather than wrapped so a caller that already holds one — from a
/// scheduler slot, a GUI job, or a Git operation — passes the same token down
/// instead of translating between two cancellation mechanisms. ADR-0001 records
/// that this one dependency on `harkness-git` is the whole reason this crate
/// names it, and that a `harkness-cancel` crate to tidy the graph was refused.
pub use harkness_git::Cancellation;

#[cfg(test)]
mod tests {
    /// Every source file in the crate, for the checks that are about what the
    /// code says rather than what it does.
    const SOURCES: &[(&str, &str)] = &[
        ("src/lib.rs", include_str!("lib.rs")),
        ("src/text.rs", include_str!("text.rs")),
        ("src/contract/mod.rs", include_str!("contract/mod.rs")),
        (
            "src/contract/capability.rs",
            include_str!("contract/capability.rs"),
        ),
        ("src/contract/error.rs", include_str!("contract/error.rs")),
        ("src/contract/event.rs", include_str!("contract/event.rs")),
        ("src/contract/ids.rs", include_str!("contract/ids.rs")),
        (
            "src/contract/message.rs",
            include_str!("contract/message.rs"),
        ),
        (
            "src/contract/provider.rs",
            include_str!("contract/provider.rs"),
        ),
        ("src/assemble/mod.rs", include_str!("assemble/mod.rs")),
        (
            "src/assemble/assembler.rs",
            include_str!("assemble/assembler.rs"),
        ),
        ("src/assemble/clock.rs", include_str!("assemble/clock.rs")),
        ("src/assemble/driver.rs", include_str!("assemble/driver.rs")),
        ("src/assemble/turn.rs", include_str!("assemble/turn.rs")),
        ("src/assemble/utf8.rs", include_str!("assemble/utf8.rs")),
        ("src/scripted/mod.rs", include_str!("scripted/mod.rs")),
        ("src/scripted/script.rs", include_str!("scripted/script.rs")),
    ];

    fn manifest() -> &'static str {
        include_str!("../Cargo.toml")
    }

    /// ADR-0001 puts this crate above `harkness-git` and below
    /// `harkness-runtime`, and forbids two directions by name: the runtime and
    /// the context engine. The front ends and the four adapter crates are
    /// forbidden for the same reason — they are above this one or sideways from
    /// it — and are listed here rather than left to a dependency cycle to
    /// catch, because no cycle exists yet: nothing above names this crate.
    ///
    /// `harkness-core` is deliberately absent. ADR-0001 does not forbid it, and
    /// [#124] may legitimately want the data-directory layout for provider
    /// profiles; adding it should be a decision, not a test failure.
    ///
    /// A plain substring search rather than a parse, which also catches a name
    /// in `[dev-dependencies]` or in a comment claiming the rule has moved.
    ///
    /// [#124]: https://github.com/fullstacktaiye/harkness/issues/124
    #[test]
    fn the_manifest_names_no_crate_this_one_must_not_depend_on() {
        for forbidden in [
            "harkness-runtime",
            "harkness-context",
            "harkness-cli",
            "harkness-gui",
            "harkness-acp",
            "harkness-mcp",
            "harkness-forge",
            "harkness-recipe",
        ] {
            assert!(
                !manifest().contains(forbidden),
                "{forbidden} appears in crates/harkness-provider/Cargo.toml; ADR-0001 puts the \
                 provider boundary below the runtime and beside the context engine",
            );
        }
    }

    /// ADR-0003 keeps the workspace synchronous: `stream` blocks on the
    /// caller's worker thread and polls a cancellation token. This is
    /// permanent, unlike the HTTP-client check below.
    #[test]
    fn nothing_here_introduces_an_async_runtime() {
        for forbidden in ["tokio", "async-std", "smol", "futures"] {
            assert!(
                !manifest().contains(forbidden),
                "{forbidden} appears in crates/harkness-provider/Cargo.toml; ADR-0003 keeps the \
                 workspace synchronous",
            );
        }
        // Assembled rather than written out, so this file does not fail its own
        // check by quoting the thing it forbids.
        let declaration = concat!("async", " fn");
        for (path, source) in SOURCES {
            assert!(
                !source.contains(declaration),
                "{path} declares an {declaration}; ADR-0003 keeps the workspace synchronous",
            );
        }
    }

    /// [#111] ships the contract, the assembler, and the scripted provider, and
    /// nothing that talks to a network: `cargo tree -p harkness-provider` shows
    /// no HTTP client. [#125] is what adds one, under ADR-0003 and ADR-0007, and
    /// revising this list is part of that issue rather than a surprise inside it.
    ///
    /// [#111]: https://github.com/fullstacktaiye/harkness/issues/111
    /// [#125]: https://github.com/fullstacktaiye/harkness/issues/125
    #[test]
    fn nothing_here_reaches_a_network() {
        for forbidden in [
            "ureq",
            "reqwest",
            "hyper",
            "isahc",
            "curl",
            "surf",
            "attohttpc",
            "rustls",
            "native-tls",
        ] {
            assert!(
                !manifest().contains(forbidden),
                "{forbidden} appears in crates/harkness-provider/Cargo.toml; #111 ships no HTTP \
                 client, and #125 is where one arrives",
            );
        }
    }

    /// ADR-0002's naming rule, enforced where it can be: a merged
    /// model-endpoint-and-coding-agent abstraction must not reappear under the
    /// name the ADR refused. Doc comments may name it — explaining what was
    /// rejected is the point — so only code lines are checked.
    #[test]
    fn no_item_is_named_after_the_abstraction_adr_0002_refused() {
        let refused = concat!("Agent", "Provider");
        for (path, source) in SOURCES {
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                assert!(
                    !line.contains(refused),
                    "{path}:{} names {refused}; ADR-0002 keeps model providers, the native agent, \
                     and external coding agents as three contracts",
                    number + 1,
                );
            }
        }
    }

    /// An unknown capability is answered by the caller's conservative floor,
    /// never by unwrapping. The accessors exist so that is easy; this makes it
    /// checkable.
    #[test]
    fn no_capability_field_is_unwrapped_anywhere_in_this_crate() {
        for (path, source) in SOURCES {
            for forbidden in [
                concat!("context_window", ".unwrap"),
                concat!("context_window", ".expect"),
                concat!("max_output_tokens", ".unwrap"),
                concat!("max_output_tokens", ".expect"),
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{path} contains {forbidden}; an unknown capability is answered by the \
                     caller's floor, not by a guess",
                );
            }
        }
    }

    /// The scripted fixtures are compiled in with `include_str!`, so the file
    /// list and the registry cannot drift — but a fixture nobody included would
    /// sit in the directory unnoticed. This is what notices.
    #[test]
    fn every_fixture_on_disk_is_compiled_into_the_registry() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scripted/fixtures");
        let mut found = std::fs::read_dir(&directory)
            .expect("the fixture directory is readable")
            .map(|entry| {
                entry
                    .expect("a readable directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        found.sort();

        let mut expected = crate::scripted::ScriptedProvider::scenario_names()
            .iter()
            .map(|name| format!("{}-v1.json", name.replace('_', "-")))
            .collect::<Vec<_>>();
        expected.sort();

        assert_eq!(found, expected);
    }
}
