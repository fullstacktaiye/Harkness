//! The durable spellings of a snapshot and a provenance record.
//!
//! These forms are frozen by committed fixtures because [#110] persists them: a
//! snapshot becomes a `workspace_snapshots` row and provenance travels in event
//! payloads. Changing a field name, a variant spelling, or a timestamp format
//! after that point is a `runtime.db` migration plus a new fixture, so the
//! fixtures exist now, before there is anything to migrate.
//!
//! Three rules carry over from the run store, for the reasons stated there:
//!
//! - Every record probes `schema_version` before its body is parsed, so a row
//!   written by a newer build reads as an upgrade request rather than as a
//!   corrupt column.
//! - The strict body rejects unknown fields at the current version. A field this
//!   build does not know is a disagreement, not something to drop on the next
//!   write.
//! - The owned deserialization type and the borrowing serialization type must
//!   stay byte-compatible, which a test asserts.
//!
//! Loading is also where a snapshot's three content digests are re-derived from
//! its entry lists and compared. A hand-edited row claiming a digest its own
//! contents do not produce fails to load rather than entering the process as an
//! identity nothing supports.
//!
//! [#110]: https://github.com/fullstacktaiye/harkness/issues/110

use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::digest::Sha256Hex;
use crate::error::ContextDomainError;
use crate::ids::SnapshotId;
use crate::path::RepoPath;
use crate::provenance::{
    ByteRange, Provenance, RankExplanation, RetrievalSource, SelectionReason, Sensitivity,
    SymbolRef,
};
use crate::snapshot::{
    CaptureRequest, FileDigestEntry, SnapshotDigest, SnapshotFiles, WorkspaceSnapshot,
};

/// Newest durable context-record schema understood by this build.
pub const CONTEXT_RECORD_SCHEMA_VERSION: u32 = 1;
/// Oldest durable context-record schema understood by this build.
pub const MINIMUM_CONTEXT_RECORD_SCHEMA_VERSION: u32 = 1;

/// Refuses a record this build cannot read, naming which direction it is out of
/// step in.
pub fn validate_record_schema_version(
    record: &'static str,
    found: u32,
) -> Result<(), ContextDomainError> {
    if found < MINIMUM_CONTEXT_RECORD_SCHEMA_VERSION {
        return Err(ContextDomainError::SchemaVersionTooOld {
            record,
            found,
            minimum: MINIMUM_CONTEXT_RECORD_SCHEMA_VERSION,
        });
    }
    if found > CONTEXT_RECORD_SCHEMA_VERSION {
        return Err(ContextDomainError::SchemaVersionTooNew {
            record,
            found,
            maximum: CONTEXT_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Strict owned representation used to deserialize a [`WorkspaceSnapshot`].
///
/// `worktree_root` is a JSON string, so a non-UTF-8 worktree root fails to
/// serialize rather than being stored lossily — the same known Unix limitation
/// the run store's workspace paths carry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SnapshotWire {
    /// Context-record schema version.
    pub schema_version: u32,
    /// Stable snapshot identifier.
    pub id: SnapshotId,
    /// Catalog project the workspace belongs to.
    pub project_id: ProjectId,
    /// The repository's shared mutation domain.
    pub repository_identity: String,
    /// Canonicalized worktree root.
    pub worktree_root: PathBuf,
    /// Checked-out commit; absent on an unborn branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Checked-out branch; absent when `HEAD` is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Staged paths and the blob ids Git holds for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged: Vec<FileDigestEntry>,
    /// Modified tracked paths and their working-tree content hashes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracked_dirty: Vec<FileDigestEntry>,
    /// Untracked eligible paths and their content hashes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked: Vec<FileDigestEntry>,
    /// Digest over `staged`.
    pub index_digest: Sha256Hex,
    /// Digest over `tracked_dirty`.
    pub tracked_dirty_digest: Sha256Hex,
    /// Digest over `untracked`.
    pub untracked_digest: Sha256Hex,
    /// Digest over the discovered instruction set.
    pub instructions_digest: Sha256Hex,
    /// Configuration generation this capture was taken under.
    pub config_generation: u64,
    /// Index generation this capture was taken against.
    pub index_generation: u64,
    /// UTC RFC 3339 capture time.
    #[serde(with = "time::serde::rfc3339")]
    pub captured_at: OffsetDateTime,
    /// The composite workspace identity, re-derived and checked on load.
    pub digest: SnapshotDigest,
}

/// Borrowing representation used to serialize a [`WorkspaceSnapshot`].
#[derive(Clone, Debug, Serialize)]
pub struct SnapshotWireRef<'a> {
    /// Context-record schema version.
    pub schema_version: u32,
    /// Stable snapshot identifier.
    pub id: SnapshotId,
    /// Catalog project the workspace belongs to.
    pub project_id: ProjectId,
    /// The repository's shared mutation domain.
    pub repository_identity: &'a str,
    /// Canonicalized worktree root.
    pub worktree_root: &'a Path,
    /// Checked-out commit; absent on an unborn branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<&'a str>,
    /// Checked-out branch; absent when `HEAD` is detached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<&'a str>,
    /// Staged paths and the blob ids Git holds for them.
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub staged: &'a [FileDigestEntry],
    /// Modified tracked paths and their working-tree content hashes.
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub tracked_dirty: &'a [FileDigestEntry],
    /// Untracked eligible paths and their content hashes.
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub untracked: &'a [FileDigestEntry],
    /// Digest over `staged`.
    pub index_digest: &'a Sha256Hex,
    /// Digest over `tracked_dirty`.
    pub tracked_dirty_digest: &'a Sha256Hex,
    /// Digest over `untracked`.
    pub untracked_digest: &'a Sha256Hex,
    /// Digest over the discovered instruction set.
    pub instructions_digest: &'a Sha256Hex,
    /// Configuration generation this capture was taken under.
    pub config_generation: u64,
    /// Index generation this capture was taken against.
    pub index_generation: u64,
    /// UTC RFC 3339 capture time.
    #[serde(with = "time::serde::rfc3339")]
    pub captured_at: OffsetDateTime,
    /// The composite workspace identity.
    pub digest: SnapshotDigest,
}

impl<'a> From<&'a WorkspaceSnapshot> for SnapshotWireRef<'a> {
    fn from(snapshot: &'a WorkspaceSnapshot) -> Self {
        Self {
            schema_version: CONTEXT_RECORD_SCHEMA_VERSION,
            id: snapshot.id(),
            project_id: snapshot.project_id(),
            repository_identity: snapshot.repository_identity(),
            worktree_root: snapshot.worktree_root(),
            head: snapshot.head(),
            branch: snapshot.branch(),
            staged: snapshot.files().staged(),
            tracked_dirty: snapshot.files().tracked_dirty(),
            untracked: snapshot.files().untracked(),
            index_digest: snapshot.index_digest(),
            tracked_dirty_digest: snapshot.tracked_dirty_digest(),
            untracked_digest: snapshot.untracked_digest(),
            instructions_digest: snapshot.instructions_digest(),
            config_generation: snapshot.config_generation(),
            index_generation: snapshot.index_generation(),
            captured_at: snapshot.captured_at(),
            digest: snapshot.digest(),
        }
    }
}

impl From<&WorkspaceSnapshot> for SnapshotWire {
    fn from(snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            schema_version: CONTEXT_RECORD_SCHEMA_VERSION,
            id: snapshot.id(),
            project_id: snapshot.project_id(),
            repository_identity: snapshot.repository_identity().to_owned(),
            worktree_root: snapshot.worktree_root().to_path_buf(),
            head: snapshot.head().map(str::to_owned),
            branch: snapshot.branch().map(str::to_owned),
            staged: snapshot.files().staged().to_vec(),
            tracked_dirty: snapshot.files().tracked_dirty().to_vec(),
            untracked: snapshot.files().untracked().to_vec(),
            index_digest: snapshot.index_digest().clone(),
            tracked_dirty_digest: snapshot.tracked_dirty_digest().clone(),
            untracked_digest: snapshot.untracked_digest().clone(),
            instructions_digest: snapshot.instructions_digest().clone(),
            config_generation: snapshot.config_generation(),
            index_generation: snapshot.index_generation(),
            captured_at: snapshot.captured_at(),
            digest: snapshot.digest(),
        }
    }
}

impl TryFrom<SnapshotWire> for WorkspaceSnapshot {
    type Error = ContextDomainError;

    fn try_from(wire: SnapshotWire) -> Result<Self, Self::Error> {
        validate_record_schema_version("workspace_snapshot", wire.schema_version)?;
        let invalid = |reason: String| ContextDomainError::InvalidSnapshotWire { reason };
        if wire.captured_at.offset() != UtcOffset::UTC {
            return Err(invalid("captured_at must use the UTC offset".to_owned()));
        }
        if wire.repository_identity.is_empty() {
            return Err(invalid("repository_identity is empty".to_owned()));
        }
        if wire.worktree_root.as_os_str().is_empty() {
            return Err(invalid("worktree_root is empty".to_owned()));
        }
        if wire.head.as_deref().is_some_and(str::is_empty) {
            return Err(invalid("head is present and empty".to_owned()));
        }
        if wire.branch.as_deref().is_some_and(str::is_empty) {
            return Err(invalid("branch is present and empty".to_owned()));
        }

        let request = CaptureRequest {
            project_id: wire.project_id,
            instructions_digest: wire.instructions_digest,
            config_generation: wire.config_generation,
            index_generation: wire.index_generation,
        };
        let snapshot = Self::assemble(
            wire.id,
            &request,
            wire.repository_identity,
            wire.worktree_root,
            wire.head,
            wire.branch,
            SnapshotFiles::new(wire.staged, wire.tracked_dirty, wire.untracked),
            wire.captured_at,
        );

        // Re-derived rather than trusted. A row whose entry list was edited, or
        // whose paths were reordered or duplicated, no longer digests to what it
        // claims, and that is exactly the row this refuses.
        check_digest("index", snapshot.index_digest(), &wire.index_digest)?;
        check_digest(
            "tracked_dirty",
            snapshot.tracked_dirty_digest(),
            &wire.tracked_dirty_digest,
        )?;
        check_digest(
            "untracked",
            snapshot.untracked_digest(),
            &wire.untracked_digest,
        )?;
        snapshot.require_digest(&wire.digest)?;
        Ok(snapshot)
    }
}

/// Refuses one component digest that disagrees with the entries beneath it.
fn check_digest(
    component: &'static str,
    expected: &Sha256Hex,
    found: &Sha256Hex,
) -> Result<(), ContextDomainError> {
    if expected == found {
        return Ok(());
    }
    Err(ContextDomainError::DigestMismatch {
        component,
        expected: expected.to_string(),
        found: found.to_string(),
    })
}

/// Strict owned representation used to deserialize a [`Provenance`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProvenanceWire {
    /// Context-record schema version.
    pub schema_version: u32,
    /// Which retrieval path produced the content.
    pub source: RetrievalSource,
    /// Repository-relative path, absent for content that has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
    /// Byte range within that path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ByteRange>,
    /// The symbol the content describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolRef>,
    /// Digest of the exact bytes the model was shown.
    pub content_sha256: Sha256Hex,
    /// The workspace state the content was read from.
    pub snapshot_id: SnapshotId,
    /// Why the content was included.
    pub reason: SelectionReason,
    /// Where it ranked, when ranking produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<RankExplanation>,
    /// Whether the shown bytes are a prefix of the source content.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// How the content must be treated.
    pub sensitivity: Sensitivity,
}

/// Borrowing representation used to serialize a [`Provenance`].
#[derive(Clone, Debug, Serialize)]
pub struct ProvenanceWireRef<'a> {
    /// Context-record schema version.
    pub schema_version: u32,
    /// Which retrieval path produced the content.
    pub source: RetrievalSource,
    /// Repository-relative path, absent for content that has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'a RepoPath>,
    /// Byte range within that path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<ByteRange>,
    /// The symbol the content describes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<&'a SymbolRef>,
    /// Digest of the exact bytes the model was shown.
    pub content_sha256: &'a Sha256Hex,
    /// The workspace state the content was read from.
    pub snapshot_id: SnapshotId,
    /// Why the content was included.
    pub reason: &'a SelectionReason,
    /// Where it ranked, when ranking produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<&'a RankExplanation>,
    /// Whether the shown bytes are a prefix of the source content.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// How the content must be treated.
    pub sensitivity: &'a Sensitivity,
}

impl<'a> From<&'a Provenance> for ProvenanceWireRef<'a> {
    fn from(record: &'a Provenance) -> Self {
        Self {
            schema_version: CONTEXT_RECORD_SCHEMA_VERSION,
            source: record.source,
            path: record.path.as_ref(),
            range: record.range,
            symbol: record.symbol.as_ref(),
            content_sha256: &record.content_sha256,
            snapshot_id: record.snapshot_id,
            reason: &record.reason,
            rank: record.rank.as_ref(),
            truncated: record.truncated,
            sensitivity: &record.sensitivity,
        }
    }
}

impl From<&Provenance> for ProvenanceWire {
    fn from(record: &Provenance) -> Self {
        Self {
            schema_version: CONTEXT_RECORD_SCHEMA_VERSION,
            source: record.source,
            path: record.path.clone(),
            range: record.range,
            symbol: record.symbol.clone(),
            content_sha256: record.content_sha256.clone(),
            snapshot_id: record.snapshot_id,
            reason: record.reason.clone(),
            rank: record.rank.clone(),
            truncated: record.truncated,
            sensitivity: record.sensitivity.clone(),
        }
    }
}

impl TryFrom<ProvenanceWire> for Provenance {
    type Error = ContextDomainError;

    fn try_from(wire: ProvenanceWire) -> Result<Self, Self::Error> {
        validate_record_schema_version("provenance", wire.schema_version)?;
        let record = Self {
            source: wire.source,
            path: wire.path,
            range: wire.range,
            symbol: wire.symbol,
            content_sha256: wire.content_sha256,
            snapshot_id: wire.snapshot_id,
            reason: wire.reason,
            rank: wire.rank,
            truncated: wire.truncated,
            sensitivity: wire.sensitivity,
        };
        record.validate()?;
        Ok(record)
    }
}

fn slice_is_empty<T>(values: &&[T]) -> bool {
    values.is_empty()
}

#[derive(Deserialize)]
struct SchemaVersionProbe {
    schema_version: u32,
}

macro_rules! impl_versioned_deserialize {
    ($wire:ty, $strict:ty, $record:literal) => {
        impl<'de> Deserialize<'de> for $wire {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = Value::deserialize(deserializer)?;
                let probe = SchemaVersionProbe::deserialize(&value).map_err(de::Error::custom)?;
                validate_record_schema_version($record, probe.schema_version)
                    .map_err(de::Error::custom)?;
                <$strict>::deserialize(value)
                    .map(Into::into)
                    .map_err(de::Error::custom)
            }
        }
    };
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotWireStrict {
    schema_version: u32,
    id: SnapshotId,
    project_id: ProjectId,
    repository_identity: String,
    worktree_root: PathBuf,
    #[serde(default)]
    head: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    staged: Vec<FileDigestEntry>,
    #[serde(default)]
    tracked_dirty: Vec<FileDigestEntry>,
    #[serde(default)]
    untracked: Vec<FileDigestEntry>,
    index_digest: Sha256Hex,
    tracked_dirty_digest: Sha256Hex,
    untracked_digest: Sha256Hex,
    instructions_digest: Sha256Hex,
    config_generation: u64,
    index_generation: u64,
    #[serde(with = "time::serde::rfc3339")]
    captured_at: OffsetDateTime,
    digest: SnapshotDigest,
}

impl From<SnapshotWireStrict> for SnapshotWire {
    fn from(wire: SnapshotWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            id: wire.id,
            project_id: wire.project_id,
            repository_identity: wire.repository_identity,
            worktree_root: wire.worktree_root,
            head: wire.head,
            branch: wire.branch,
            staged: wire.staged,
            tracked_dirty: wire.tracked_dirty,
            untracked: wire.untracked,
            index_digest: wire.index_digest,
            tracked_dirty_digest: wire.tracked_dirty_digest,
            untracked_digest: wire.untracked_digest,
            instructions_digest: wire.instructions_digest,
            config_generation: wire.config_generation,
            index_generation: wire.index_generation,
            captured_at: wire.captured_at,
            digest: wire.digest,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceWireStrict {
    schema_version: u32,
    source: RetrievalSource,
    #[serde(default)]
    path: Option<RepoPath>,
    #[serde(default)]
    range: Option<ByteRange>,
    #[serde(default)]
    symbol: Option<SymbolRef>,
    content_sha256: Sha256Hex,
    snapshot_id: SnapshotId,
    reason: SelectionReason,
    #[serde(default)]
    rank: Option<RankExplanation>,
    #[serde(default)]
    truncated: bool,
    sensitivity: Sensitivity,
}

impl From<ProvenanceWireStrict> for ProvenanceWire {
    fn from(wire: ProvenanceWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            source: wire.source,
            path: wire.path,
            range: wire.range,
            symbol: wire.symbol,
            content_sha256: wire.content_sha256,
            snapshot_id: wire.snapshot_id,
            reason: wire.reason,
            rank: wire.rank,
            truncated: wire.truncated,
            sensitivity: wire.sensitivity,
        }
    }
}

impl_versioned_deserialize!(SnapshotWire, SnapshotWireStrict, "workspace_snapshot");
impl_versioned_deserialize!(ProvenanceWire, ProvenanceWireStrict, "provenance");

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;

    use harkness_core::ProjectId;
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use time::OffsetDateTime;

    use super::{
        CONTEXT_RECORD_SCHEMA_VERSION, ProvenanceWire, ProvenanceWireRef, SnapshotWire,
        SnapshotWireRef,
    };
    use crate::digest::{Sha256Hex, empty_path_set_digest};
    use crate::ids::SnapshotId;
    use crate::path::RepoPath;
    use crate::probe::ContentDigest;
    use crate::provenance::{
        ByteRange, Provenance, RankExplanation, RankSignal, RetrievalSource, SelectionReason,
        SelectionReasonKind, Sensitivity, SymbolRef,
    };
    use crate::snapshot::{CaptureRequest, FileDigestEntry, SnapshotFiles, WorkspaceSnapshot};

    fn project_id() -> ProjectId {
        ProjectId::from_str("11111111-1111-4111-8111-111111111111").unwrap()
    }

    fn snapshot_id() -> SnapshotId {
        SnapshotId::from_str("22222222-2222-4222-8222-222222222222").unwrap()
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn path(text: &str) -> RepoPath {
        RepoPath::from_bytes(text.as_bytes().to_vec())
    }

    fn snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot::assemble(
            snapshot_id(),
            &CaptureRequest::new(project_id()).with_index_generation(3),
            "9c2f4c8a-0d3a-5a4e-9c1c-6f9b7c1d2e3f".to_owned(),
            PathBuf::from("/workspaces/harkness"),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            Some("main".to_owned()),
            SnapshotFiles::new(
                vec![FileDigestEntry::new(
                    path("staged.txt"),
                    ContentDigest::StagedBlob("abcdef0123456789".to_owned()),
                )],
                vec![FileDigestEntry::new(
                    path("src/main.rs"),
                    ContentDigest::of_content(b"fn main() {}"),
                )],
                vec![FileDigestEntry::new(
                    path("notes/scratch.md"),
                    ContentDigest::Unreadable,
                )],
            ),
            at(1),
        )
    }

    fn provenance() -> Provenance {
        Provenance::new(
            RetrievalSource::LexicalSearch,
            snapshot_id(),
            b"fn main() {}",
            SelectionReason::new(SelectionReasonKind::QueryMatch, "matched 'main'"),
        )
        .at_path(path("src/main.rs"))
        .in_range(ByteRange::new(0, 12).with_lines(1, 1))
        .for_symbol(SymbolRef::new(
            &path("src/main.rs"),
            "rust",
            "main",
            "function",
        ))
        .ranked(
            RankExplanation::new(0.75, 0, 4)
                .with_signals(vec![RankSignal::new("lexical", 0.9, 0.5)]),
        )
        .truncated()
        .with_sensitivity(Sensitivity::suspicious("«untrusted-content»"))
    }

    fn assert_fixture(wire: impl Serialize, fixture: &str) {
        let actual = format!("{}\n", serde_json::to_string_pretty(&wire).unwrap());
        assert_eq!(actual, fixture);
    }

    fn assert_owned_fixture<T>(fixture: &str)
    where
        T: DeserializeOwned + Serialize,
    {
        let decoded = serde_json::from_str::<T>(fixture).unwrap();
        assert_eq!(
            format!("{}\n", serde_json::to_string_pretty(&decoded).unwrap()),
            fixture
        );
    }

    #[test]
    #[ignore = "run explicitly to rewrite the frozen fixtures after a deliberate format change"]
    fn regenerate_the_frozen_v1_fixtures() {
        for (name, json) in [
            (
                "workspace-snapshot-v1.json",
                serde_json::to_string_pretty(&SnapshotWireRef::from(&snapshot())).unwrap(),
            ),
            (
                "provenance-v1.json",
                serde_json::to_string_pretty(&ProvenanceWireRef::from(&provenance())).unwrap(),
            ),
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/fixtures")
                .join(name);
            std::fs::write(path, format!("{json}\n")).unwrap();
        }
    }

    #[test]
    fn frozen_v1_json_fixtures_cover_every_record_type() {
        assert_fixture(
            SnapshotWireRef::from(&snapshot()),
            include_str!("fixtures/workspace-snapshot-v1.json"),
        );
        assert_fixture(
            ProvenanceWireRef::from(&provenance()),
            include_str!("fixtures/provenance-v1.json"),
        );
        assert_owned_fixture::<SnapshotWire>(include_str!("fixtures/workspace-snapshot-v1.json"));
        assert_owned_fixture::<ProvenanceWire>(include_str!("fixtures/provenance-v1.json"));
    }

    #[test]
    fn borrowing_and_owned_wire_forms_are_byte_compatible() {
        let snapshot = snapshot();
        assert_eq!(
            serde_json::to_string(&SnapshotWireRef::from(&snapshot)).unwrap(),
            serde_json::to_string(&SnapshotWire::from(&snapshot)).unwrap()
        );
        let provenance = provenance();
        assert_eq!(
            serde_json::to_string(&ProvenanceWireRef::from(&provenance)).unwrap(),
            serde_json::to_string(&ProvenanceWire::from(&provenance)).unwrap()
        );
    }

    #[test]
    fn a_snapshot_round_trips_through_its_wire_form() {
        let snapshot = snapshot();
        let json = serde_json::to_string(&SnapshotWireRef::from(&snapshot)).unwrap();
        let wire = serde_json::from_str::<SnapshotWire>(&json).unwrap();
        let decoded = WorkspaceSnapshot::try_from(wire).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.digest(), snapshot.digest());
    }

    #[test]
    fn a_provenance_record_round_trips_through_its_wire_form() {
        let record = provenance();
        let json = serde_json::to_string(&ProvenanceWireRef::from(&record)).unwrap();
        let wire = serde_json::from_str::<ProvenanceWire>(&json).unwrap();
        assert_eq!(Provenance::try_from(wire).unwrap(), record);
    }

    #[test]
    fn a_clean_snapshot_omits_its_empty_entry_lists() {
        let clean = WorkspaceSnapshot::assemble(
            snapshot_id(),
            &CaptureRequest::new(project_id()),
            "identity".to_owned(),
            PathBuf::from("/workspaces/harkness"),
            None,
            Some("main".to_owned()),
            SnapshotFiles::default(),
            at(0),
        );
        let value = serde_json::to_value(SnapshotWireRef::from(&clean)).unwrap();
        assert!(value.get("staged").is_none());
        assert!(value.get("head").is_none());
        assert_eq!(
            value["index_digest"],
            json!(empty_path_set_digest().to_string())
        );
        assert_eq!(
            WorkspaceSnapshot::try_from(serde_json::from_value::<SnapshotWire>(value).unwrap())
                .unwrap(),
            clean
        );
    }

    #[test]
    fn unknown_fields_are_rejected_at_the_current_version() {
        for fixture in [
            include_str!("fixtures/workspace-snapshot-v1.json"),
            include_str!("fixtures/provenance-v1.json"),
        ] {
            let mut value: Value = serde_json::from_str(fixture).unwrap();
            value["surprise"] = json!(true);
            assert!(
                serde_json::from_value::<SnapshotWire>(value.clone()).is_err()
                    && serde_json::from_value::<ProvenanceWire>(value).is_err()
            );
        }
    }

    #[test]
    fn a_future_schema_version_reads_as_an_upgrade_request() {
        let mut value: Value =
            serde_json::from_str(include_str!("fixtures/workspace-snapshot-v1.json")).unwrap();
        value["schema_version"] = json!(CONTEXT_RECORD_SCHEMA_VERSION + 1);
        let error = serde_json::from_value::<SnapshotWire>(value).unwrap_err();
        assert!(error.to_string().contains("upgrade Harkness"), "{error}");

        let mut value: Value =
            serde_json::from_str(include_str!("fixtures/provenance-v1.json")).unwrap();
        value["schema_version"] = json!(0);
        let error = serde_json::from_value::<ProvenanceWire>(value).unwrap_err();
        assert!(error.to_string().contains("older than"), "{error}");
    }

    #[test]
    fn a_row_whose_digest_disagrees_with_its_contents_is_refused() {
        let snapshot = snapshot();
        let mut wire = SnapshotWire::from(&snapshot);
        wire.untracked.clear();
        let error = WorkspaceSnapshot::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "digest_mismatch");
        assert!(error.to_string().contains("untracked"), "{error}");
    }

    #[test]
    fn a_row_whose_composite_digest_was_edited_is_refused() {
        let snapshot = snapshot();
        let mut wire = SnapshotWire::from(&snapshot);
        wire.config_generation += 1;
        let error = WorkspaceSnapshot::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "digest_mismatch");
        assert!(error.to_string().contains("workspace_snapshot"), "{error}");
    }

    #[test]
    fn a_reordered_entry_list_is_refused_rather_than_silently_normalized() {
        let snapshot = snapshot();
        let mut wire = SnapshotWire::from(&snapshot);
        wire.untracked.push(FileDigestEntry::new(
            path("aaa.txt"),
            ContentDigest::of_content(b"a"),
        ));
        let error = WorkspaceSnapshot::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "digest_mismatch");
    }

    #[test]
    fn a_non_utc_timestamp_is_refused() {
        let snapshot = snapshot();
        let mut wire = SnapshotWire::from(&snapshot);
        wire.captured_at = wire
            .captured_at
            .to_offset(time::UtcOffset::from_hms(2, 0, 0).unwrap());
        let error = WorkspaceSnapshot::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "invalid_snapshot_wire");
    }

    #[test]
    fn empty_identity_strings_are_refused() {
        for mutate in [
            (|wire: &mut SnapshotWire| wire.repository_identity.clear()) as fn(&mut SnapshotWire),
            |wire: &mut SnapshotWire| wire.worktree_root = PathBuf::new(),
            |wire: &mut SnapshotWire| wire.head = Some(String::new()),
            |wire: &mut SnapshotWire| wire.branch = Some(String::new()),
        ] {
            let mut wire = SnapshotWire::from(&snapshot());
            mutate(&mut wire);
            let error = WorkspaceSnapshot::try_from(wire).unwrap_err();
            assert_eq!(error.kind(), "invalid_snapshot_wire");
        }
    }

    #[test]
    fn a_provenance_row_is_revalidated_on_load() {
        let record = provenance();
        let mut wire = ProvenanceWire::from(&record);
        wire.path = None;
        let error = Provenance::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "invalid_provenance_wire");
    }

    #[test]
    fn a_malformed_digest_column_fails_to_decode() {
        let mut value: Value =
            serde_json::from_str(include_str!("fixtures/workspace-snapshot-v1.json")).unwrap();
        value["index_digest"] = json!("not-a-digest");
        assert!(serde_json::from_value::<SnapshotWire>(value).is_err());
        assert!("not-a-digest".parse::<Sha256Hex>().is_err());
    }
}
