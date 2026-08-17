//! The typed language the Harkness context engine speaks.
//!
//! Everything the engine will later say about a piece of repository content —
//! "this chunk came from this file, at this hash, under this workspace state,
//! selected for this reason" — is a value of a type defined here. Nothing in
//! this crate indexes, searches, or retrieves anything; it fixes the vocabulary
//! those components have to speak, before there are components to disagree.
//!
//! # The identity model
//!
//! Two workspaces are the same workspace when their [`WorkspaceSnapshot::digest`]
//! values are equal. That digest covers ten components, and the reason it is not
//! simply `HEAD` is that `HEAD` is wrong in entirely ordinary ways: an editor
//! buffer is saved and `HEAD` does not move; two linked worktrees sit at one
//! commit with different uncommitted work; a file is staged and never committed.
//! ADR-0008 records the alternatives that were refused. Concretely, identity is:
//!
//! | Component | Separates |
//! | --- | --- |
//! | repository identity | unrelated repositories sharing a path or a commit |
//! | worktree root | two linked worktrees of one repository |
//! | `HEAD` | different committed bases |
//! | branch | a detached checkout from a branch at the same commit |
//! | index digest | staged work, which `HEAD` does not describe |
//! | tracked-dirty digest | the uncommitted edits `HEAD` misses most often |
//! | untracked digest | new files the model will read |
//! | instruction-set digest | guidance that changed mid-run |
//! | config generation | a different view of the same tree |
//! | index generation | chunk references invalidated by a rebuild |
//!
//! A [`SnapshotId`] is not that digest. Capturing one unchanged workspace twice
//! produces two ids and one digest: the id names the *capture*, and the digest
//! names the *workspace*. Provenance records the id, because a run inspected
//! later wants to know which capture it read; staleness checks compare the
//! digest, because what matters then is whether the bytes moved.
//!
//! # Reading and re-reading
//!
//! [`WorkspaceSnapshot::capture`] reads the components once.
//! [`WorkspaceSnapshot::verify`] re-reads the cheap ones and returns a
//! [`FreshnessState`] naming every diverged path, so a mutation planned against
//! a snapshot can be refused with a reason instead of landing as a
//! plausible-looking bad diff. Capture is deliberately tolerant of a workspace
//! that moves underneath it — an unreadable file contributes a sentinel and a
//! diagnostic rather than failing the capture — because a snapshot is an honest
//! record of what was read, and verification is what turns that into safety.
//!
//! # The service boundary
//!
//! [`ContextEngine`] is the one way anything reaches a context feature, and
//! [`index`] is the disposable per-repository cache beneath it. The engine
//! returns typed values and persists nothing: a snapshot becomes evidence only
//! when `harkness-runtime` records it, which is what keeps deleting
//! `<data_dir>/context/` lossless (ADR-0004). Of the eight facade methods,
//! [`snapshot`](ContextEngine::snapshot) and
//! [`inventory`](ContextEngine::inventory) are implemented; the rest return
//! [`ContextEngineError::NotYetAvailable`] naming what is missing, so every
//! later retrieval issue has a compiling seam.
//!
//! # The eligible-file inventory
//!
//! [`InventoryBuilder::build`] turns one captured snapshot into a
//! [`FileInventory`]: the bounded, classified set of files that worktree offers
//! as context. It is the only walk — chunking, indexing, search, the repository
//! map, and instruction discovery all read its entries rather than the
//! filesystem — which is what stops two retrieval features disagreeing about
//! whether a file exists.
//!
//! Four layers decide what it holds, in order, first opinion winning: the
//! built-in [`BUILT_IN_DENIALS`], the global user ignore file, the repository's
//! own ignore file — which may only tighten — and the repository's `.gitignore`
//! chain. A credential-bearing path is stopped by layer one, counted in
//! [`FileInventory::denied_count`] and recorded nowhere, so no later stage has
//! anything to retrieve. Each recorded path then carries exactly one
//! [`FileClass`], assigned by [`FileSample::classify`].
//!
//! `docs/context-inventory.md` is the reference: the whole denial list, every
//! class with its heuristics, and how a repository tightens what Harkness reads.
//!
//! # What is not here
//!
//! No chunking ([#113]), no index content tables ([#114]), no search ([#116]),
//! no symbol extraction ([#117]), and no provider or token concepts ([#111],
//! [#122]). The identifiers and the facade signatures those issues need are
//! defined here so that none of them has to invent one.
//!
//! [#111]: https://github.com/fullstacktaiye/harkness/issues/111
//! [#112]: https://github.com/fullstacktaiye/harkness/issues/112
//! [#113]: https://github.com/fullstacktaiye/harkness/issues/113
//! [#114]: https://github.com/fullstacktaiye/harkness/issues/114
//! [#116]: https://github.com/fullstacktaiye/harkness/issues/116
//! [#117]: https://github.com/fullstacktaiye/harkness/issues/117
//! [#122]: https://github.com/fullstacktaiye/harkness/issues/122

#![warn(missing_docs)]

mod classify;
mod digest;
mod engine;
mod error;
mod ids;
pub mod index;
mod inventory;
mod path;
mod probe;
mod provenance;
mod snapshot;
mod wire;

pub use classify::{
    BINARY_SNIFF_BYTES, CLASSIFY_VERSION, FileClass, FileSample, OVERSIZED_FILE_THRESHOLD,
};
pub use digest::{Sha256Hex, empty_path_set_digest};
pub use engine::{
    ChunkContent, ContextEngine, ContextEngineConfig, ContextPack, InstructionSet, InventoryRequest,
    MapRequest, PackRequest, RepositoryMap, SearchQuery, SearchResults, SettingGroup, SettingOrigin,
    SettingOrigins, SymbolQuery, SymbolResults,
};
pub use error::{ContextDomainError, ContextEngineError};
pub use ids::{
    ChunkId, ContextItemId, ContextPackId, ContextQueryId, FileVersionId, SnapshotId, SymbolId,
};
pub use inventory::{
    BUILT_IN_DENIALS, Boundary, FileInventory, GLOBAL_IGNORE_FILE, IgnoreLayer, InventoryBuilder,
    InventoryDiagnostic, InventoryEntry, InventoryError, InventoryPolicy, InventoryTruncation,
    MAX_DIAGNOSTIC_TEXT_BYTES, MAX_IGNORE_FILE_BYTES, MAX_INVENTORY_DIAGNOSTICS,
    MAX_INVENTORY_FILES, MAX_WALK_DURATION, REPOSITORY_IGNORE_FILE,
};
pub use path::RepoPath;
pub use probe::{
    ContentDigest, FilesystemProbe, ProbeFailure, UnreadablePath, UntrackedExpansion,
    WorkspaceProbe,
};
pub use provenance::{
    ByteRange, MAX_PROVENANCE_TEXT_BYTES, Provenance, RankExplanation, RankSignal, RetrievalSource,
    SelectionReason, SelectionReasonKind, Sensitivity, SymbolRef,
};
pub use snapshot::{
    Capture, CaptureDiagnostics, CaptureRequest, FileDigestEntry, FreshnessState, PathDivergence,
    SkippedPath, SnapshotComponent, SnapshotDigest, SnapshotFiles, StalePath, UnverifiableReason,
    Verification, WorkspaceReading, WorkspaceSnapshot,
};
pub use wire::{
    CONTEXT_RECORD_SCHEMA_VERSION, MINIMUM_CONTEXT_RECORD_SCHEMA_VERSION, ProvenanceWire,
    ProvenanceWireRef, SnapshotWire, SnapshotWireRef, validate_record_schema_version,
};

#[cfg(test)]
mod tests {
    /// The dependency direction is the crate boundary ADR-0001 fixes: the
    /// runtime depends on the context engine, never the reverse. A `use` that
    /// inverted it would be caught by the compiler, but a manifest entry added
    /// "just for a test helper" would not be, and it is the manifest that decides
    /// whether a snapshot can be captured without a run store in the process.
    #[test]
    fn the_manifest_never_depends_on_the_runtime() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("the crate manifest is readable");
        assert!(
            !manifest.contains("harkness-runtime"),
            "harkness-context must not depend on harkness-runtime:\n{manifest}"
        );
    }
}
