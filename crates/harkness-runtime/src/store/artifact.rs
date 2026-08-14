//! Content too large for a row, and the metadata that finds it again.
//!
//! A build log, a full diff, a downloaded file, an event payload that outgrew
//! its column: none of these belong in SQLite, where they would inflate every
//! query that touches the table and defeat the page cache. They live as files
//! under `<data_dir>/artifacts/<run_id>/<artifact_id>`, with one row in
//! `artifacts` recording what they are.
//!
//! # File first, row second
//!
//! Finalizing an artifact is *write, sync, rename, then insert*. Every other
//! ordering can produce a metadata row pointing at bytes that were never made
//! durable, and a reader has no way to tell that row from a good one. The
//! ordering here can only produce two harmless outcomes:
//!
//! | Crash point | What is left | What a reader sees |
//! | --- | --- | --- |
//! | before the rename | a `.tmp-` file | nothing; readers only ever name `<artifact_id>` |
//! | between rename and insert | an orphan file | nothing; no row refers to it |
//! | after the insert | a row and its file | the artifact |
//!
//! An orphan file costs disk and nothing else. Collecting them is deliberately
//! out of scope here — a garbage collector that ran before the insert committed
//! would delete live artifacts, so it is a job for a later pass that can reason
//! about which runs are finished.
//!
//! That table is about *crashes*. An ordinary refusal is not one: a rejected
//! transition, an insert naming another run's step, a database that stayed busy
//! — each returns an error from a perfectly healthy store, and a caller
//! retrying one must not leave a file per attempt behind. Every such path
//! removes what it wrote, whether that is [`ArtifactSink::finish`] cleaning up
//! after a refused insert or
//! [`Store::append_event`](super::Store::append_event) cleaning up a spill.
//!
//! # A missing file degrades a read, never fails one
//!
//! Deleting an artifact's bytes from outside Harkness is something a user may
//! simply do. [`Store::artifact`](super::Store::artifact) stats the file and
//! reports [`Availability::Missing`] or [`Availability::SizeMismatch`]; loading
//! the run and reading its event log are untouched, because neither of them
//! opens an artifact. Only asking for the *content* fails, and it fails naming
//! the artifact.
//!
//! # The stored path is checked, not trusted
//!
//! `storage_path` is derivable from `(run_id, id)`. It is stored anyway, so the
//! layout is legible from a database alone, and every read compares it against
//! the derived form: a row edited to name `../../.ssh/id_rsa` is refused with
//! [`StoreError::ForbiddenArtifactPath`] rather than opened. The path Harkness
//! actually uses is always rebuilt from the two identifiers, never joined from
//! the stored text, so even the comparison is belt to the derivation's braces.
//!
//! # Streaming
//!
//! [`ArtifactSink`] is an [`io::Write`], and that is the whole of its write
//! surface: there is no method taking a whole artifact, so no caller can be
//! tempted into holding one in memory. Bytes pass through the redactor, then
//! through a hasher and a counter, then into the file, so the recorded size and
//! SHA-256 describe exactly the bytes on disk. Memory is one buffer regardless
//! of artifact size.
//!
//! The label and media type go through the redactor too, by value rather than
//! by stream. They are caller text that becomes durable in a column, and an
//! artifact named after the credential a tool just leaked would otherwise
//! persist it in the one place redaction never looks.
//!
//! # Containment
//!
//! An artifact may name a step or a tool call only of the run it claims. The
//! composite foreign keys in migration 2 enforce that in the database, exactly
//! as migration 1 does for a tool call's denormalized run, so no Rust path has
//! to re-check it and none can forget to.

use std::borrow::Cow;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::{Connection, Row, named_params};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::domain::{ArtifactId, RUNTIME_RECORD_SCHEMA_VERSION, RunId, StepId, ToolCallId};
use crate::tool::{ArtifactRef, ArtifactStream, ArtifactWriter, ToolError};

use super::Store;
use super::column::{decode_id, decode_timestamp, encode_text, encode_timestamp};
use super::error::{Containment, StoreError, insert_failed, query_failed};
use super::repository::{optional_text, schema_version, text};

const ARTIFACT: &str = "artifact";

/// Directory holding artifact content inside the Harkness data directory.
///
/// A sibling of `repositories/`, `worktrees/` and `locks/`, and covered by the
/// same `HARKNESS_DATA_DIR` override.
pub const ARTIFACTS_DIRECTORY: &str = "artifacts";

/// Bytes buffered between the redactor and the file.
///
/// The only memory an artifact write is allowed to scale with is this constant.
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

const ARTIFACT_COLUMNS: &str = "schema_version, id, run_id, step_id, tool_call_id, name, \
     media_type, byte_size, sha256, storage_path, created_at, availability";

/// Whether an artifact's bytes are still where they were recorded.
///
/// Probed on every metadata read rather than trusted from the column, because
/// the interesting failure — someone deleted the file — happens outside
/// Harkness and leaves nothing to update the column.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Availability {
    /// The file is present and its size matches what was recorded.
    Available,
    /// The file is gone.
    Missing,
    /// The file is present but is no longer the size that was recorded.
    ///
    /// Distinguished from [`Missing`](Self::Missing) because it means something
    /// rewrote the content, which a provenance check should treat differently
    /// from a file that was cleaned up.
    SizeMismatch,
}

impl Availability {
    /// Every availability in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Available, Self::Missing, Self::SizeMismatch];

    /// The stable spelling stored in the `availability` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Missing => "missing",
            Self::SizeMismatch => "size_mismatch",
        }
    }
}

impl fmt::Display for Availability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A recorded artifact: what it is, how large, and whether it is still there.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Artifact {
    id: ArtifactId,
    run_id: RunId,
    step_id: Option<StepId>,
    tool_call_id: Option<ToolCallId>,
    name: String,
    media_type: String,
    byte_size: u64,
    sha256: String,
    created_at: OffsetDateTime,
    availability: Availability,
}

impl Artifact {
    /// Identity of the artifact, which also names its file.
    #[must_use]
    pub const fn id(&self) -> ArtifactId {
        self.id
    }

    /// Run the artifact was recorded against.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Step the artifact came from, when it came from one.
    #[must_use]
    pub const fn step_id(&self) -> Option<StepId> {
        self.step_id
    }

    /// Tool call the artifact came from, when it came from one.
    #[must_use]
    pub const fn tool_call_id(&self) -> Option<ToolCallId> {
        self.tool_call_id
    }

    /// Caller-facing label, such as `build.log`.
    ///
    /// Never a path component; the file on disk is named by the identity.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// IANA media type the content was stored under.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Size of the stored content in bytes.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Hex SHA-256 of the stored content.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// When the artifact was finalized, in UTC.
    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }

    /// Whether the bytes are still where they were recorded.
    #[must_use]
    pub const fn availability(&self) -> Availability {
        self.availability
    }

    /// A reference a tool can return inside its own output.
    #[must_use]
    pub fn reference(&self) -> ArtifactRef {
        ArtifactRef {
            id: self.id.to_string(),
            media_type: self.media_type.clone(),
            byte_len: self.byte_size,
        }
    }
}

/// The relative location an artifact's bytes are stored at.
///
/// Forward slashes on every platform, because the value is stored in a database
/// that is copied between them. Nothing joins this string onto a path; it exists
/// to be written to the column and compared against on the way back.
fn relative_path(run_id: RunId, id: ArtifactId) -> String {
    format!("{ARTIFACTS_DIRECTORY}/{run_id}/{id}")
}

/// The directory holding one run's artifacts.
fn run_directory(data_dir: &Path, run_id: RunId) -> PathBuf {
    data_dir.join(ARTIFACTS_DIRECTORY).join(run_id.to_string())
}

/// Where an artifact's bytes live, rebuilt from the two identities.
pub(super) fn artifact_path(data_dir: &Path, run_id: RunId, id: ArtifactId) -> PathBuf {
    run_directory(data_dir, run_id).join(id.to_string())
}

/// Bytes on their way to disk, hashed and counted as they pass.
///
/// The file is held in an [`Option`] so sealing can close it while the writer
/// chain above still exists: renaming a file another handle has open fails on
/// Windows, and a redactor that wrapped the sink owns part of that chain.
#[derive(Debug)]
struct Recorded {
    file: Option<File>,
    hasher: Sha256,
    byte_size: u64,
}

/// A cloneable handle onto the recording layer.
#[derive(Clone, Debug)]
struct RecordedHandle(Arc<Mutex<Recorded>>);

impl RecordedHandle {
    fn locked(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Write for RecordedHandle {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut recorded = self.locked();
        let Some(file) = recorded.file.as_mut() else {
            return Err(io::Error::other(
                "the artifact sink was closed before this write",
            ));
        };
        // `write_all` rather than `write`, so the hash and the count can never
        // describe more bytes than the file received.
        file.write_all(buffer)?;
        recorded.hasher.update(buffer);
        recorded.byte_size += buffer.len() as u64;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.locked().file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// An artifact whose bytes are durable but whose row has not been written.
///
/// The two halves of finalization are separable so a test can stop between
/// them and prove the crash matrix in this module's documentation.
#[derive(Clone, Debug)]
pub(super) struct SealedArtifact {
    id: ArtifactId,
    run_id: RunId,
    step_id: Option<StepId>,
    tool_call_id: Option<ToolCallId>,
    name: String,
    media_type: String,
    byte_size: u64,
    sha256: String,
    created_at: OffsetDateTime,
}

impl SealedArtifact {
    /// Identity the artifact will be recorded under.
    pub(super) const fn id(&self) -> ArtifactId {
        self.id
    }

    /// Run the artifact belongs to, which is half of where its bytes live.
    pub(super) const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Media type the content was stored under.
    pub(super) fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Size of the stored content in bytes.
    pub(super) const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Hex SHA-256 of the stored content.
    pub(super) fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Whether an artifact's bytes still owe the redactor a pass.
///
/// Content that arrived from a caller is [`Pending`](Self::Pending) and is
/// scrubbed by [`Redactor::wrap_stream`](super::Redactor::wrap_stream) as it
/// streams. Content the store itself produced from an already-redacted value is
/// [`Applied`](Self::Applied): it has been through
/// [`redact_text`](super::Redactor::redact_text) once, on the strings inside it,
/// and wrapping it again would redact twice.
///
/// This is not a way to skip redaction, and there is no public route to it. It
/// exists because a spilled event payload has one job — to hold exactly what the
/// row would have held had it fit — and a second pass would break that for any
/// rule that is not idempotent, while a rule implemented only in `redact_text`
/// would leave a payload scrubbed under the threshold and bare above it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Redaction {
    /// Caller bytes; wrap the sink.
    Pending,
    /// Bytes the store already redacted by value; do not wrap the sink.
    Applied,
}

/// Where a sink's bytes currently are, and therefore what abandoning it must
/// clean up.
///
/// A boolean cannot express this. The rename is the moment the bytes change
/// name, so a failure *after* it owes the destination a removal and a failure
/// before it owes the temporary file one — and asking "is it sealed" gets the
/// second answer for both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Staging {
    /// Under the `.tmp-` name; no reader resolves it.
    Temporary,
    /// Under the final name, with nothing yet responsible for it.
    Final,
    /// A [`SealedArtifact`] owns the bytes now; the sink must not touch them.
    Released,
}

/// How a sink reaches the store it writes into.
///
/// A sink is ordinarily a short-lived borrow of the store a caller already
/// holds. A sink handed to a *tool* is not: it is opened by the execution
/// context and then moved onto the reader thread that fills it, which outlives
/// any borrow the caller could lend. Both are the same sink, so the store handle
/// is what varies rather than the type around it.
enum StoreHandle<'a> {
    Borrowed(&'a Store),
    Owned(Arc<Store>),
}

impl std::ops::Deref for StoreHandle<'_> {
    type Target = Store;

    fn deref(&self) -> &Store {
        match self {
            Self::Borrowed(store) => store,
            Self::Owned(store) => store,
        }
    }
}

/// A streaming write into the artifact store.
///
/// Write to it like any other [`io::Write`], then [`finish`](Self::finish) it.
/// Dropping it without finishing removes whatever it wrote: an abandoned write
/// leaves neither a row nor bytes anybody could mistake for an artifact.
pub struct ArtifactSink<'a> {
    store: StoreHandle<'a>,
    id: ArtifactId,
    run_id: RunId,
    step_id: Option<StepId>,
    tool_call_id: Option<ToolCallId>,
    name: String,
    media_type: String,
    created_at: OffsetDateTime,
    temporary: PathBuf,
    destination: PathBuf,
    stream: Option<Box<dyn Write + Send>>,
    recorded: RecordedHandle,
    staging: Staging,
}

impl ArtifactSink<'static> {
    /// Opens a sink that keeps the store alive for as long as it exists.
    ///
    /// The shape a tool's artifact writer needs: the sink is handed to whichever
    /// thread produces the content and finished there, long after any borrow the
    /// opening call could have lent it.
    pub(super) fn create_owned(
        store: Arc<Store>,
        run_id: RunId,
        name: &str,
        media_type: &str,
        created_at: OffsetDateTime,
        redaction: Redaction,
    ) -> Result<Self, StoreError> {
        Self::open(
            StoreHandle::Owned(store),
            run_id,
            name,
            media_type,
            created_at,
            redaction,
        )
    }
}

impl<'a> ArtifactSink<'a> {
    pub(super) fn create(
        store: &'a Store,
        run_id: RunId,
        name: &str,
        media_type: &str,
        created_at: OffsetDateTime,
        redaction: Redaction,
    ) -> Result<Self, StoreError> {
        Self::open(
            StoreHandle::Borrowed(store),
            run_id,
            name,
            media_type,
            created_at,
            redaction,
        )
    }

    fn open(
        store: StoreHandle<'a>,
        run_id: RunId,
        name: &str,
        media_type: &str,
        created_at: OffsetDateTime,
        redaction: Redaction,
    ) -> Result<Self, StoreError> {
        // The label and the media type are caller text that becomes durable in
        // a column, so they go through the redactor like everything else here —
        // a tool naming its artifact after the token it just leaked would
        // otherwise persist it somewhere redaction never looks. Store-generated
        // values arrive `Applied` and are left exactly as this module wrote
        // them, so a constant like the spilled-payload media type still reads
        // back as the constant.
        let (name, media_type) = match redaction {
            Redaction::Pending => (
                store.redactor().redact_text(name),
                store.redactor().redact_text(media_type),
            ),
            Redaction::Applied => (Cow::Borrowed(name), Cow::Borrowed(media_type)),
        };
        // Both land in bounded columns, so they are refused here rather than
        // after a whole artifact has been streamed to disk — and after
        // redaction, because a rule may lengthen what it rewrites.
        encode_text(ARTIFACT, "name", &name)?;
        encode_text(ARTIFACT, "media_type", &media_type)?;
        // A row can only be inserted against a stored run, and finding that out
        // now costs one index seek instead of a whole wasted stream.
        require_run(&store, run_id)?;

        let id = ArtifactId::new();
        let directory = run_directory(store.data_dir(), run_id);
        create_private_dir(&directory)?;
        let temporary = directory.join(format!(".tmp-{id}"));
        let file = create_private_file(&temporary)?;

        let recorded = RecordedHandle(Arc::new(Mutex::new(Recorded {
            file: Some(file),
            hasher: Sha256::new(),
            byte_size: 0,
        })));
        // Redactor on the outside, recording on the inside: what is hashed and
        // counted is what the file receives, not what the caller handed over.
        let buffered: Box<dyn Write + Send> = Box::new(io::BufWriter::with_capacity(
            STREAM_BUFFER_BYTES,
            recorded.clone(),
        ));
        let stream = match redaction {
            Redaction::Pending => store.redactor().wrap_stream(buffered),
            Redaction::Applied => buffered,
        };

        let destination = artifact_path(store.data_dir(), run_id, id);
        Ok(Self {
            store,
            id,
            run_id,
            step_id: None,
            tool_call_id: None,
            name: name.into_owned(),
            media_type: media_type.into_owned(),
            created_at,
            temporary,
            destination,
            stream: Some(stream),
            recorded,
            staging: Staging::Temporary,
        })
    }

    /// Where the bytes are staged before the rename.
    ///
    /// The path a failure during streaming should name: the destination does not
    /// exist yet, and reporting it would send a reader looking in the wrong
    /// place.
    pub(super) fn temporary(&self) -> &Path {
        &self.temporary
    }

    /// Attributes the artifact to a step of the same run.
    #[must_use]
    pub fn for_step(mut self, step_id: StepId) -> Self {
        self.step_id = Some(step_id);
        self
    }

    /// Attributes the artifact to a tool call of the same run.
    #[must_use]
    pub fn for_tool_call(mut self, tool_call_id: ToolCallId) -> Self {
        self.tool_call_id = Some(tool_call_id);
        self
    }

    /// Identity the artifact will be recorded under.
    ///
    /// Known before the first byte is written, so a caller can reference the
    /// artifact in the same event that announces it.
    #[must_use]
    pub const fn id(&self) -> ArtifactId {
        self.id
    }

    /// Makes the bytes durable and records the artifact.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ArtifactIo`] when the content cannot be flushed,
    /// synced, or renamed, and the errors of an insert when the row cannot be
    /// written.
    pub fn finish(mut self) -> Result<Artifact, StoreError> {
        let sealed = self.seal()?;
        let recorded = self
            .store
            .in_write_transaction("recording an artifact", |connection| {
                insert_artifact(connection, &sealed)
            });
        if recorded.is_err() {
            // Sealing released the bytes, so `Drop` will leave them alone. An
            // insert refused for an ordinary reason — an association naming
            // another run, a database that stayed busy — is not the crash the
            // orphan-file trade-off is about, and a caller retrying it must not
            // leave a full copy of its content behind each time.
            let _ = fs::remove_file(&self.destination);
        }
        recorded
    }

    /// Makes the bytes durable and stops, leaving no row behind.
    ///
    /// This is the first half of [`finish`](Self::finish), separated so a test
    /// can stand where a crash would.
    pub(super) fn seal(&mut self) -> Result<SealedArtifact, StoreError> {
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| artifact_io("sealing an artifact", &self.temporary, closed()))?;
        stream
            .flush()
            .map_err(|error| artifact_io("flushing an artifact", &self.temporary, error))?;
        drop(stream);

        let (byte_size, digest) = {
            let mut recorded = self.recorded.locked();
            if let Some(file) = recorded.file.as_mut() {
                file.sync_all()
                    .map_err(|error| artifact_io("syncing an artifact", &self.temporary, error))?;
            }
            // Closing the file here rather than waiting for the sink to drop is
            // what makes the rename below work on Windows even if the redactor's
            // wrapper is still holding a handle onto the chain.
            recorded.file = None;
            (recorded.byte_size, recorded.hasher.clone().finalize())
        };

        fs::rename(&self.temporary, &self.destination)
            .map_err(|error| artifact_io("renaming an artifact", &self.destination, error))?;
        // From here the bytes answer to their final name, and anything that
        // fails below owes *that* path a removal — the temporary one no longer
        // exists, so cleaning it up would tidy nothing and orphan the artifact.
        self.staging = Staging::Final;

        // The bytes are already durable; what the rename still needs is a sync
        // of the directory holding the new entry. Windows has no equivalent
        // handle to sync, exactly as the project catalog's write does not.
        #[cfg(unix)]
        if let Some(directory) = self.destination.parent() {
            File::open(directory)
                .and_then(|handle| handle.sync_all())
                .map_err(|error| artifact_io("syncing the artifact directory", directory, error))?;
        }

        // The returned record is now what accounts for the bytes; whoever holds
        // it decides whether they survive.
        self.staging = Staging::Released;
        Ok(SealedArtifact {
            id: self.id,
            run_id: self.run_id,
            step_id: self.step_id,
            tool_call_id: self.tool_call_id,
            name: std::mem::take(&mut self.name),
            media_type: std::mem::take(&mut self.media_type),
            byte_size,
            sha256: hex(&digest),
            created_at: self.created_at,
        })
    }
}

impl Write for ArtifactSink<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.stream.as_mut() {
            Some(stream) => stream.write(buffer),
            None => Err(closed()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.stream.as_mut() {
            Some(stream) => stream.flush(),
            None => Err(closed()),
        }
    }
}

impl Drop for ArtifactSink<'_> {
    /// Removes whatever the sink wrote, wherever it currently is.
    ///
    /// Which path that is depends on how far the seal got, which is why the
    /// staging state has three values rather than being a flag: a write
    /// abandoned before the rename owes the `.tmp-` file, one abandoned after it
    /// owes the destination, and one whose sealed record has been handed to a
    /// caller owes nothing — cleaning that up would delete the bytes the caller
    /// is about to record.
    ///
    /// A failure to remove the file is not worth turning into a panic during
    /// unwind; the worst case is the orphan the crash matrix already allows for.
    fn drop(&mut self) {
        let abandoned = match self.staging {
            Staging::Temporary => &self.temporary,
            Staging::Final => &self.destination,
            Staging::Released => return,
        };
        self.recorded.locked().file = None;
        let _ = fs::remove_file(abandoned);
    }
}

impl fmt::Debug for ArtifactSink<'_> {
    /// Omits the writer chain, which is an opaque trait object.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactSink")
            .field("id", &self.id)
            .field("run_id", &self.run_id)
            .field("name", &self.name)
            .field("media_type", &self.media_type)
            .finish_non_exhaustive()
    }
}

/// An [`ArtifactWriter`] backed by a store, for a tool's execution context.
///
/// The bridge between the tool contract and the streaming store beneath it.
/// Both shapes a tool can produce — a value it already holds and a stream it is
/// still generating — go through one sink, so a build log stored a chunk at a
/// time is redacted, hashed and named exactly as a diff stored whole.
#[derive(Clone, Debug)]
pub struct StoreArtifacts {
    store: Arc<Store>,
    run_id: RunId,
    step_id: Option<StepId>,
    tool_call_id: Option<ToolCallId>,
}

impl StoreArtifacts {
    /// Writes artifacts for one tool call into `store`.
    #[must_use]
    pub const fn new(
        store: Arc<Store>,
        run_id: RunId,
        step_id: StepId,
        tool_call_id: ToolCallId,
    ) -> Self {
        Self {
            store,
            run_id,
            step_id: Some(step_id),
            tool_call_id: Some(tool_call_id),
        }
    }

    fn open_sink(&self, name: &str, media_type: &str) -> Result<ArtifactSink<'static>, StoreError> {
        let mut sink = ArtifactSink::create_owned(
            Arc::clone(&self.store),
            self.run_id,
            name,
            media_type,
            OffsetDateTime::now_utc(),
            Redaction::Pending,
        )?;
        if let Some(step_id) = self.step_id {
            sink = sink.for_step(step_id);
        }
        if let Some(tool_call_id) = self.tool_call_id {
            sink = sink.for_tool_call(tool_call_id);
        }
        Ok(sink)
    }
}

impl ArtifactWriter for StoreArtifacts {
    fn redact_text(&self, text: &str) -> String {
        self.store.redactor().redact_text(text).into_owned()
    }

    /// Opens a store-backed stream for the tool to fill.
    ///
    /// A storage failure is `execution_failed` rather than a partial success: a
    /// tool that believes it stored a log and returns a reference to nothing is
    /// worse than one that reports it could not.
    fn open(&mut self, name: &str, media_type: &str) -> Result<Box<dyn ArtifactStream>, ToolError> {
        let sink =
            self.open_sink(name, media_type)
                .map_err(|error| ToolError::ExecutionFailed {
                    message: format!("{name:?} could not be stored as an artifact: {error}"),
                })?;
        Ok(Box::new(StoreArtifactStream {
            name: name.to_owned(),
            sink,
        }))
    }
}

/// One tool artifact being streamed into the store.
struct StoreArtifactStream {
    /// The caller's label, kept only so a failure names what it was storing.
    name: String,
    sink: ArtifactSink<'static>,
}

impl Write for StoreArtifactStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.sink.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

impl ArtifactStream for StoreArtifactStream {
    fn finish(self: Box<Self>) -> Result<ArtifactRef, ToolError> {
        let name = self.name;
        self.sink
            .finish()
            .map(|artifact| artifact.reference())
            .map_err(|error| ToolError::ExecutionFailed {
                message: format!("{name:?} could not be stored as an artifact: {error}"),
            })
    }
}

// -- statements --------------------------------------------------------------

pub(super) fn insert_artifact(
    connection: &Connection,
    sealed: &SealedArtifact,
) -> Result<Artifact, StoreError> {
    let byte_size = i64::try_from(sealed.byte_size).map_err(|_| StoreError::ColumnEncoding {
        record: ARTIFACT,
        field: "byte_size",
        reason: format!("{} is beyond the storable size range", sealed.byte_size),
    })?;
    connection
        .execute(
            &format!(
                "INSERT INTO artifacts ({ARTIFACT_COLUMNS}) VALUES (:schema_version, :id, :run_id, \
                 :step_id, :tool_call_id, :name, :media_type, :byte_size, :sha256, :storage_path, \
                 :created_at, :availability)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": sealed.id.to_string(),
                ":run_id": sealed.run_id.to_string(),
                ":step_id": sealed.step_id.map(|id| id.to_string()),
                ":tool_call_id": sealed.tool_call_id.map(|id| id.to_string()),
                ":name": encode_text(ARTIFACT, "name", &sealed.name)?,
                ":media_type": encode_text(ARTIFACT, "media_type", &sealed.media_type)?,
                ":byte_size": byte_size,
                ":sha256": sealed.sha256.as_str(),
                ":storage_path": relative_path(sealed.run_id, sealed.id),
                ":created_at": encode_timestamp(ARTIFACT, "created_at", sealed.created_at)?,
                ":availability": Availability::Available.as_str(),
            },
        )
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: ARTIFACT,
                    // As for an event: the step and tool-call keys are composite
                    // with `run_id`, so this also covers naming one of another
                    // run's.
                    parent: "run, or a step or tool call of that run,",
                },
                &sealed.id,
                "recording an artifact",
                error,
            )
        })?;

    Ok(Artifact {
        id: sealed.id,
        run_id: sealed.run_id,
        step_id: sealed.step_id,
        tool_call_id: sealed.tool_call_id,
        name: sealed.name.clone(),
        media_type: sealed.media_type.clone(),
        byte_size: sealed.byte_size,
        sha256: sealed.sha256.clone(),
        created_at: sealed.created_at,
        availability: Availability::Available,
    })
}

pub(super) fn load_artifact(
    connection: &Connection,
    data_dir: &Path,
    id: ArtifactId,
) -> Result<Artifact, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE id = :id"
        ))
        .map_err(|error| query_failed("preparing the artifact query", error))?;
    let artifact = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(artifact_from_row(row, data_dir))
        })
        .map_err(|error| {
            if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                return StoreError::NotFound {
                    record: ARTIFACT,
                    id: id.to_string(),
                };
            }
            query_failed("loading an artifact", error)
        })??;
    Ok(artifact)
}

pub(super) fn load_run_artifacts(
    connection: &Connection,
    data_dir: &Path,
    run_id: RunId,
) -> Result<Vec<Artifact>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE run_id = :run_id \
             ORDER BY created_at, id"
        ))
        .map_err(|error| query_failed("preparing the run artifact query", error))?;
    let rows = statement
        .query_map(named_params! { ":run_id": run_id.to_string() }, |row| {
            Ok(artifact_from_row(row, data_dir))
        })
        .map_err(|error| query_failed("listing the artifacts of a run", error))?;
    let mut artifacts = Vec::new();
    for row in rows {
        artifacts.push(row.map_err(|error| query_failed("reading an artifact row", error))??);
    }
    Ok(artifacts)
}

fn artifact_from_row(row: &Row<'_>, data_dir: &Path) -> Result<Artifact, StoreError> {
    schema_version(row, ARTIFACT)?;
    let id: ArtifactId = decode_id(ARTIFACT, "id", &text(row, ARTIFACT, "id")?)?;
    let run_id: RunId = decode_id(ARTIFACT, "run_id", &text(row, ARTIFACT, "run_id")?)?;

    // The one check that has to happen before anything else touches the
    // filesystem: a row naming somewhere other than its own reserved location
    // is a tampered row, not an artifact.
    let stored_path = text(row, ARTIFACT, "storage_path")?;
    if stored_path != relative_path(run_id, id) {
        return Err(StoreError::ForbiddenArtifactPath {
            id: id.to_string(),
            path: stored_path,
        });
    }

    let byte_size = row
        .get::<_, i64>("byte_size")
        .map_err(|error| column(ARTIFACT, "byte_size", &error))?;
    let byte_size = u64::try_from(byte_size).map_err(|_| StoreError::ColumnEncoding {
        record: ARTIFACT,
        field: "byte_size",
        reason: format!("{byte_size} is not a representable size"),
    })?;

    let recorded = decode_availability(&text(row, ARTIFACT, "availability")?)?;
    Ok(Artifact {
        id,
        run_id,
        step_id: optional_text(row, ARTIFACT, "step_id")?
            .map(|stored| decode_id(ARTIFACT, "step_id", &stored))
            .transpose()?,
        tool_call_id: optional_text(row, ARTIFACT, "tool_call_id")?
            .map(|stored| decode_id(ARTIFACT, "tool_call_id", &stored))
            .transpose()?,
        name: text(row, ARTIFACT, "name")?,
        media_type: text(row, ARTIFACT, "media_type")?,
        byte_size,
        sha256: text(row, ARTIFACT, "sha256")?,
        created_at: decode_timestamp(ARTIFACT, "created_at", &text(row, ARTIFACT, "created_at")?)?,
        availability: probe(recorded, &artifact_path(data_dir, run_id, id), byte_size),
    })
}

/// Refines the recorded availability against what is on disk.
///
/// The column records what was true at finalization. Only the filesystem knows
/// what is true now, and it is cheap to ask: one `stat` per artifact, and never
/// on the path that loads a run or reads its events.
fn probe(recorded: Availability, path: &Path, byte_size: u64) -> Availability {
    if recorded != Availability::Available {
        return recorded;
    }
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() == byte_size => Availability::Available,
        Ok(_) => Availability::SizeMismatch,
        Err(_) => Availability::Missing,
    }
}

fn decode_availability(stored: &str) -> Result<Availability, StoreError> {
    Availability::ALL
        .iter()
        .copied()
        .find(|availability| availability.as_str() == stored)
        .ok_or_else(|| StoreError::ColumnEncoding {
            record: ARTIFACT,
            field: "availability",
            reason: format!("{stored} is not an availability this build understands"),
        })
}

/// Refuses an artifact against a run that is not stored.
fn require_run(store: &Store, run_id: RunId) -> Result<(), StoreError> {
    let exists = store.with_reader(|connection| {
        connection
            .query_row(
                "SELECT 1 FROM runs WHERE id = :id",
                named_params! { ":id": run_id.to_string() },
                |_| Ok(()),
            )
            .map(|()| true)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(query_failed("checking the run of an artifact", other)),
            })
    })?;
    if exists {
        return Ok(());
    }
    Err(StoreError::MissingParent {
        record: ARTIFACT,
        id: run_id.to_string(),
        parent: "run",
    })
}

// -- filesystem plumbing ------------------------------------------------------

/// Creates a directory only its owner can traverse.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| artifact_io("creating the artifact directory", path, error))
}

/// Creates the artifact directory.
///
/// Windows has no mode to set here; access is governed by the inherited ACL of
/// the data directory, exactly as it is for the database beside it.
#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path)
        .map_err(|error| artifact_io("creating the artifact directory", path, error))
}

/// Creates a file only its owner can read.
///
/// Process output, Git stderr and tool errors all end up here, so the default
/// umask is not a strong enough claim about who may read them.
fn create_private_file(path: &Path) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| artifact_io("creating an artifact file", path, error))
}

fn closed() -> io::Error {
    io::Error::other("the artifact sink has already been finished")
}

fn artifact_io(operation: &'static str, path: &Path, source: io::Error) -> StoreError {
    StoreError::ArtifactIo {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn column(record: &'static str, field: &'static str, error: &rusqlite::Error) -> StoreError {
    StoreError::ColumnEncoding {
        record,
        field,
        reason: error.to_string(),
    }
}

/// Renders a digest as the lowercase hex the column holds.
fn hex(digest: &[u8]) -> String {
    use fmt::Write as _;
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut rendered, byte| {
            let _ = write!(rendered, "{byte:02x}");
            rendered
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::domain::{ArtifactId, RunId};

    use super::{ARTIFACTS_DIRECTORY, Availability, artifact_path, hex, relative_path};

    #[test]
    fn availability_spellings_are_stable_and_distinct() {
        let spellings = Availability::ALL
            .iter()
            .map(|availability| availability.as_str())
            .collect::<Vec<_>>();
        assert_eq!(spellings, ["available", "missing", "size_mismatch"]);
        assert_eq!(Availability::Missing.to_string(), "missing");
    }

    #[test]
    fn a_stored_path_is_derived_from_the_two_identities() {
        let run_id = RunId::new();
        let id = ArtifactId::new();

        assert_eq!(
            relative_path(run_id, id),
            format!("{ARTIFACTS_DIRECTORY}/{run_id}/{id}")
        );
        // The path actually used is rebuilt component by component, so nothing
        // a column holds is ever joined onto the data directory.
        assert_eq!(
            artifact_path(std::path::Path::new("/data"), run_id, id),
            std::path::Path::new("/data")
                .join(ARTIFACTS_DIRECTORY)
                .join(run_id.to_string())
                .join(id.to_string())
        );
    }

    #[test]
    fn digests_render_as_lowercase_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
