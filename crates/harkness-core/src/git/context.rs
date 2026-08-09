//! Bounded, blob-addressed file and hunk context.
//!
//! A diff records the object ID of each content side. Those IDs are the stable
//! address here: index and revision content is read from the immutable object
//! database even if a path changes later. The one exception is a working-tree
//! side, whose recorded ID is only a hash. That source is read by path, passed
//! through the same built-in clean filters as the diff when applicable, and
//! accepted only when that representation still hashes to the recorded ID.
//!
//! Retrieval is local, read-only inspection. It uses libgit2 in process, takes
//! no repository lock and never spawns system Git.

use std::{
    ffi::{CString, OsStr},
    fs::{self, File},
    io::{self, Read, Write},
    os::raw::{c_char, c_int},
    path::{Path, PathBuf},
    ptr, slice,
};

use git2::{Binding, ErrorCode, ObjectType, Oid, Repository};
use libgit2_sys as raw;
use tempfile::NamedTempFile;

use crate::git::{
    DEFAULT_MAX_DIFF_FILE_SIZE, DEFAULT_MAX_DIFF_TOTAL_BYTES, DiffLine, DiffLineKind, DiffTarget,
    FileDiff, GitError, Hunk, commit,
};

/// Which content side of a diff a context response represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileSide {
    Old,
    New,
}

/// The line range to retrieve from one file side.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileContextRange {
    /// Return the complete file.
    FullFile,
    /// Return one recorded hunk plus additional source lines around it.
    ///
    /// `start_line` and `line_count` are the coordinates for the requested
    /// [`FileSide`]. `lines_before` and `lines_after` extend that recorded
    /// range; they are additional to the context already present in the hunk.
    Hunk {
        start_line: u32,
        line_count: u32,
        lines_before: u32,
        lines_after: u32,
    },
}

/// Where file-context bytes come from.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileContextSource {
    /// An immutable blob in the repository object database.
    Blob { blob_id: String },
    /// The empty content of a diff side that does not exist.
    ///
    /// `blob_id` is the all-zero object ID recorded by [`FileDiff`]. It is a
    /// sentinel, not an object that is looked up in the repository.
    Absent { blob_id: String },
    /// The one diff side that has a hash but no stored blob.
    ///
    /// `expected_blob_id` is the content hash recorded by the diff. The path is
    /// used only to reproduce the same raw or clean-filtered representation,
    /// never as its identity.
    Worktree {
        path: PathBuf,
        expected_blob_id: String,
    },
}

impl FileContextSource {
    fn blob_id(&self) -> &str {
        match self {
            Self::Blob { blob_id } | Self::Absent { blob_id } => blob_id,
            Self::Worktree {
                expected_blob_id, ..
            } => expected_blob_id,
        }
    }
}

/// One bounded request for file content.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileContextRequest {
    pub source: FileContextSource,
    pub side: FileSide,
    pub range: FileContextRange,
    /// Largest source file that may return content bytes.
    pub max_file_size: u64,
    /// Largest selected line range that may return content bytes.
    pub max_total_bytes: u64,
}

impl FileContextRequest {
    /// Builds a request for an immutable blob.
    #[must_use]
    pub fn blob(blob_id: impl Into<String>, side: FileSide, range: FileContextRange) -> Self {
        Self {
            source: FileContextSource::Blob {
                blob_id: blob_id.into(),
            },
            side,
            range,
            max_file_size: DEFAULT_MAX_DIFF_FILE_SIZE,
            max_total_bytes: DEFAULT_MAX_DIFF_TOTAL_BYTES,
        }
    }

    /// Builds a request for the absent side of an addition or deletion.
    ///
    /// Front ends reconstructing a request from a serialized diff record use
    /// this constructor when the selected side has no path and carries the
    /// all-zero object ID sentinel.
    #[must_use]
    pub fn absent(blob_id: impl Into<String>, side: FileSide, range: FileContextRange) -> Self {
        Self {
            source: FileContextSource::Absent {
                blob_id: blob_id.into(),
            },
            side,
            range,
            max_file_size: DEFAULT_MAX_DIFF_FILE_SIZE,
            max_total_bytes: DEFAULT_MAX_DIFF_TOTAL_BYTES,
        }
    }

    /// Builds a full-file request for an immutable blob.
    #[must_use]
    pub fn full_blob(blob_id: impl Into<String>, side: FileSide) -> Self {
        Self::blob(blob_id, side, FileContextRange::FullFile)
    }

    /// Builds a request for working-tree bytes guarded by their recorded hash.
    #[must_use]
    pub fn worktree(
        path: impl Into<PathBuf>,
        expected_blob_id: impl Into<String>,
        side: FileSide,
        range: FileContextRange,
    ) -> Self {
        Self {
            source: FileContextSource::Worktree {
                path: path.into(),
                expected_blob_id: expected_blob_id.into(),
            },
            side,
            range,
            max_file_size: DEFAULT_MAX_DIFF_FILE_SIZE,
            max_total_bytes: DEFAULT_MAX_DIFF_TOTAL_BYTES,
        }
    }

    /// Retrieves the complete selected side of a [`FileDiff`].
    ///
    /// Stored sides are addressed only by blob ID. The new side of a target
    /// that ends at the working tree retains its path solely for the stale-safe
    /// fallback described by [`FileContextSource::Worktree`].
    #[must_use]
    pub fn full_file(file: &FileDiff, side: FileSide) -> Self {
        Self::from_file(file, side, FileContextRange::FullFile)
    }

    /// Retrieves a recorded hunk plus `lines_before` and `lines_after`.
    ///
    /// The additions are relative to the hunk's existing context. For example,
    /// expanding a hunk produced with three context lines by two on each side
    /// returns the same source span as that hunk with five context lines, until
    /// the file boundary is reached.
    #[must_use]
    pub fn for_hunk(
        file: &FileDiff,
        hunk: &Hunk,
        side: FileSide,
        lines_before: u32,
        lines_after: u32,
    ) -> Self {
        let (start_line, line_count) = match side {
            FileSide::Old => (hunk.old_start, hunk.old_lines),
            FileSide::New => (hunk.new_start, hunk.new_lines),
        };
        Self::from_file(
            file,
            side,
            FileContextRange::Hunk {
                start_line,
                line_count,
                lines_before,
                lines_after,
            },
        )
    }

    /// Sets the largest source file that may return content.
    #[must_use]
    pub fn with_max_file_size(mut self, max_file_size: u64) -> Self {
        self.max_file_size = max_file_size;
        self
    }

    /// Sets the byte budget for the selected line range.
    #[must_use]
    pub fn with_max_total_bytes(mut self, max_total_bytes: u64) -> Self {
        self.max_total_bytes = max_total_bytes;
        self
    }

    fn from_file(file: &FileDiff, side: FileSide, range: FileContextRange) -> Self {
        let (blob_id, path) = match side {
            FileSide::Old => (&file.old_blob_id, file.old_path.as_ref()),
            FileSide::New => (&file.new_blob_id, file.new_path.as_ref()),
        };
        if path.is_none() && !blob_id.is_empty() && blob_id.bytes().all(|byte| byte == b'0') {
            Self::absent(blob_id, side, range)
        } else if side == FileSide::New
            && matches!(
                file.target,
                DiffTarget::Unstaged | DiffTarget::RevisionAgainstWorktree { .. }
            )
            && let Some(path) = path
        {
            Self::worktree(path, blob_id, side, range)
        } else {
            Self::blob(blob_id, side, range)
        }
    }
}

/// Why a file-context request returned only its summary.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileContextOmission {
    /// The source exceeded [`FileContextRequest::max_file_size`].
    FileTooLarge { limit: u64 },
    /// The selected range exceeded [`FileContextRequest::max_total_bytes`].
    ContentBudgetExhausted { limit: u64 },
}

/// One complete line-range response or a named bounded summary.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileContextResponse {
    /// Normalized full object ID, or the verified working-tree content hash.
    pub blob_id: String,
    pub side: FileSide,
    /// The range the caller requested.
    pub range: FileContextRange,
    /// Size of the complete addressed representation in bytes.
    pub byte_size: u64,
    /// Number of lines in the complete source, absent when the source was
    /// rejected before it could be inspected within its byte bound.
    pub total_lines: Option<u32>,
    /// First returned one-based line number, absent for an empty or omitted
    /// response.
    pub start_line: Option<u32>,
    /// Byte-exact source lines. Newlines are retained exactly as in [`DiffLine`].
    /// Every line has kind [`DiffLineKind::Context`] and populates only the line
    /// number belonging to [`Self::side`].
    pub lines: Vec<DiffLine>,
    /// Named reason requested content was withheld. An empty or out-of-file
    /// range may legitimately have no lines and no omission.
    pub omission: Option<FileContextOmission>,
}

pub(crate) fn load(
    root: &Path,
    request: &FileContextRequest,
) -> Result<FileContextResponse, GitError> {
    match &request.source {
        FileContextSource::Blob { .. } => {
            let repository = commit::open(root)?;
            let id = parse_blob_id(&repository, request.source.blob_id())?;
            load_blob(&repository, root, id, request)
        }
        FileContextSource::Absent { blob_id } => {
            let repository = commit::open(root)?;
            let id = parse_blob_id(&repository, request.source.blob_id())?;
            load_absent(blob_id, id, request)
        }
        FileContextSource::Worktree { path, .. } => {
            // Reject escapes before opening or otherwise inspecting the
            // repository so path safety is independent of repository state.
            let resolved = validate_worktree_path(root, path)?;
            let repository = commit::open(root)?;
            let id = parse_blob_id(&repository, request.source.blob_id())?;
            load_worktree(&repository, path, &resolved, id, request)
        }
    }
}

struct WorktreePath {
    absolute: PathBuf,
    repository_relative: PathBuf,
}

/// Validates the path without following its final component.
///
/// A tracked symlink is content in its own right, and its target may
/// legitimately be outside the repository. Parent components still have to
/// resolve beneath the worktree so a nested symlink cannot redirect the read.
fn validate_worktree_path(root: &Path, path: &Path) -> Result<WorktreePath, GitError> {
    let repository = fs::canonicalize(root).map_err(|_| GitError::NotARepository {
        path: root.to_path_buf(),
    })?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repository.join(path)
    };
    let Some(file_name) = candidate.file_name() else {
        return Err(outside_repository(root, path));
    };
    let Some(parent) = candidate.parent() else {
        return Err(outside_repository(root, path));
    };
    let parent = fs::canonicalize(parent).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            stale(path)
        } else {
            GitError::DiffContent {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !parent.starts_with(&repository) {
        return Err(outside_repository(root, path));
    }
    let absolute = parent.join(file_name);
    let repository_relative = absolute
        .strip_prefix(&repository)
        .map_err(|_| outside_repository(root, path))?
        .to_path_buf();
    Ok(WorktreePath {
        absolute,
        repository_relative,
    })
}

fn outside_repository(root: &Path, path: &Path) -> GitError {
    GitError::PathOutsideRepository {
        path: path.to_path_buf(),
        repository: root.to_path_buf(),
    }
}

fn parse_blob_id(repository: &Repository, blob_id: &str) -> Result<Oid, GitError> {
    let parsed = Oid::from_str_ext(blob_id, repository.object_format()).map_err(|_| {
        GitError::InvalidBlobId {
            blob_id: blob_id.to_owned(),
        }
    })?;
    // `Oid::from_str_ext` also accepts abbreviated input. Context is addressed
    // by the complete ID recorded in a diff, never by a moving abbreviation.
    if parsed.to_string() != blob_id.to_ascii_lowercase() {
        return Err(GitError::InvalidBlobId {
            blob_id: blob_id.to_owned(),
        });
    }
    Ok(parsed)
}

fn load_blob(
    repository: &Repository,
    root: &Path,
    id: Oid,
    request: &FileContextRequest,
) -> Result<FileContextResponse, GitError> {
    let odb = repository
        .odb()
        .map_err(|source| inspection(root, source))?;
    let (size, kind) = odb
        .read_header(id)
        .map_err(|source| blob_lookup_error(root, id, source))?;
    if kind != ObjectType::Blob {
        return Err(GitError::BlobNotFound {
            blob_id: id.to_string(),
        });
    }
    let byte_size = u64::try_from(size).unwrap_or(u64::MAX);
    if byte_size > request.max_file_size {
        return Ok(omitted(
            request,
            id,
            byte_size,
            FileContextOmission::FileTooLarge {
                limit: request.max_file_size,
            },
        ));
    }
    // A full-file request can be refused from the object header alone. Do not
    // inflate a blob that cannot fit the caller's response budget.
    if matches!(request.range, FileContextRange::FullFile) && byte_size > request.max_total_bytes {
        return Ok(omitted(
            request,
            id,
            byte_size,
            FileContextOmission::ContentBudgetExhausted {
                limit: request.max_total_bytes,
            },
        ));
    }
    let blob = repository
        .find_blob(id)
        .map_err(|source| blob_lookup_error(root, id, source))?;
    project_lines(request, id, blob.content())
}

fn load_absent(
    blob_id: &str,
    id: Oid,
    request: &FileContextRequest,
) -> Result<FileContextResponse, GitError> {
    if !id.is_zero() {
        return Err(GitError::InvalidBlobId {
            blob_id: blob_id.to_owned(),
        });
    }
    project_lines(request, id, &[])
}

fn load_worktree(
    repository: &Repository,
    path: &Path,
    resolved: &WorktreePath,
    expected: Oid,
    request: &FileContextRequest,
) -> Result<FileContextResponse, GitError> {
    let metadata =
        fs::symlink_metadata(&resolved.absolute).map_err(|source| io_or_stale(path, source))?;

    if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(&resolved.absolute).map_err(|source| io_or_stale(path, source))?;
        let content = os_str_bytes(target.as_os_str()).to_vec();
        refuse_stale_bytes(repository, path, expected, &content)?;
        let byte_size = content.len() as u64;
        if byte_size > request.max_file_size {
            return Ok(omitted(
                request,
                expected,
                byte_size,
                FileContextOmission::FileTooLarge {
                    limit: request.max_file_size,
                },
            ));
        }
        return project_lines(request, expected, &content);
    }

    if !metadata.file_type().is_file() {
        return Err(stale(path));
    }

    if let Some(filtered) = clean_filtered_file(repository, path, resolved)?
        && let Some(response) =
            load_filtered_worktree(repository, path, expected, request, filtered)?
    {
        return Ok(response);
    }

    // An oversized working-tree source still has to prove it is the content
    // the diff recorded. Hashing streams through libgit2 and does not place the
    // file in the response model.
    if metadata.len() > request.max_file_size {
        refuse_stale_file(repository, path, &resolved.absolute, expected)?;
        let current_size = fs::symlink_metadata(&resolved.absolute)
            .map_err(|source| io_or_stale(path, source))?
            .len();
        if current_size > request.max_file_size {
            return Ok(omitted(
                request,
                expected,
                current_size,
                FileContextOmission::FileTooLarge {
                    limit: request.max_file_size,
                },
            ));
        }
    }

    let content = read_bounded(path, &resolved.absolute, request.max_file_size)?;
    if content.len() as u64 > request.max_file_size {
        // The file grew after the metadata check. Revalidate the complete file
        // before returning a size summary so changed content is still stale.
        refuse_stale_file(repository, path, &resolved.absolute, expected)?;
        let byte_size = fs::symlink_metadata(&resolved.absolute)
            .map_err(|source| io_or_stale(path, source))?
            .len()
            .max(content.len() as u64);
        return Ok(omitted(
            request,
            expected,
            byte_size,
            FileContextOmission::FileTooLarge {
                limit: request.max_file_size,
            },
        ));
    }
    refuse_stale_bytes(repository, path, expected, &content)?;
    project_lines(request, expected, &content)
}

fn load_filtered_worktree(
    repository: &Repository,
    path: &Path,
    expected: Oid,
    request: &FileContextRequest,
    filtered: NamedTempFile,
) -> Result<Option<FileContextResponse>, GitError> {
    let byte_size = filtered
        .as_file()
        .metadata()
        .map_err(|source| GitError::DiffContent {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    let actual = Oid::hash_file_ext(
        ObjectType::Blob,
        filtered.path(),
        repository.object_format(),
    )
    .map_err(|source| inspection(path, source))?;
    if actual != expected {
        // A diff that omitted content before asking libgit2 for a patch records
        // the raw worktree hash because no filtered ID was materialized. Fall
        // back to that representation; its own validation still rejects a
        // genuinely changed file.
        return Ok(None);
    }

    if byte_size > request.max_file_size {
        return Ok(Some(omitted(
            request,
            expected,
            byte_size,
            FileContextOmission::FileTooLarge {
                limit: request.max_file_size,
            },
        )));
    }
    if matches!(request.range, FileContextRange::FullFile) && byte_size > request.max_total_bytes {
        return Ok(Some(omitted(
            request,
            expected,
            byte_size,
            FileContextOmission::ContentBudgetExhausted {
                limit: request.max_total_bytes,
            },
        )));
    }
    let content = read_bounded(path, filtered.path(), request.max_file_size)?;
    project_lines(request, expected, &content).map(Some)
}

/// Applies the same built-in clean filters libgit2 used to form diff lines.
///
/// Most paths have no filters and stay on the direct, allocation-bounded read
/// path. Filtered output is streamed to an automatically removed temporary
/// file so validation remains bounded in memory even when the request only
/// needs an over-limit summary.
fn clean_filtered_file(
    repository: &Repository,
    path: &Path,
    resolved: &WorktreePath,
) -> Result<Option<NamedTempFile>, GitError> {
    let path_bytes = repository_path_bytes(&resolved.repository_relative).ok_or_else(|| {
        GitError::DiffContent {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "repository path cannot be represented for Git filtering",
            ),
        }
    })?;
    let filter_path = CString::new(path_bytes).map_err(|source| GitError::DiffContent {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, source),
    })?;
    let Some(filters) = FilterList::load(repository, path, &filter_path)? else {
        return Ok(None);
    };
    let absolute_path =
        CString::new(filesystem_path_bytes(&resolved.absolute).ok_or_else(|| {
            GitError::DiffContent {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worktree path cannot be represented for Git filtering",
                ),
            }
        })?)
        .map_err(|source| GitError::DiffContent {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, source),
        })?;
    let mut filtered = NamedTempFile::new().map_err(|source| GitError::DiffContent {
        path: path.to_path_buf(),
        source,
    })?;
    let (code, write_error) = {
        let mut sink = FilterSink::new(filtered.as_file_mut());
        // SAFETY: `filters` and `repository` own valid libgit2 handles, the
        // CString and sink outlive this synchronous call, and `FilterSink`
        // begins with the exact `git_writestream` layout libgit2 expects.
        let code = unsafe {
            git_filter_list_stream_file(
                filters.0,
                repository.raw(),
                absolute_path.as_ptr(),
                &mut sink.stream,
            )
        };
        (code, sink.error.take())
    };
    if let Some(source) = write_error {
        return Err(GitError::DiffContent {
            path: path.to_path_buf(),
            source,
        });
    }
    if code < 0 {
        return Err(filter_error(path, git2::Error::last_error(code)));
    }
    filtered
        .as_file_mut()
        .flush()
        .map_err(|source| GitError::DiffContent {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(Some(filtered))
}

#[cfg(unix)]
fn repository_path_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Some(path.as_os_str().as_bytes().to_vec())
}

#[cfg(unix)]
fn filesystem_path_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Some(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn repository_path_bytes(path: &Path) -> Option<Vec<u8>> {
    Some(path.to_str()?.replace('\\', "/").into_bytes())
}

#[cfg(not(unix))]
fn filesystem_path_bytes(path: &Path) -> Option<Vec<u8>> {
    Some(path.to_str()?.as_bytes().to_vec())
}

#[repr(C)]
struct RawFilterList {
    _private: [u8; 0],
}

struct FilterList(*mut RawFilterList);

impl FilterList {
    fn load(
        repository: &Repository,
        path: &Path,
        path_string: &CString,
    ) -> Result<Option<Self>, GitError> {
        let mut filters = ptr::null_mut();
        // SAFETY: the repository and CString are valid for the duration of the
        // call, and libgit2 initializes `filters` on success.
        let code = unsafe {
            git_filter_list_load(
                &mut filters,
                repository.raw(),
                ptr::null_mut(),
                path_string.as_ptr(),
                GIT_FILTER_TO_ODB,
                GIT_FILTER_ALLOW_UNSAFE,
            )
        };
        if code < 0 {
            return Err(filter_error(path, git2::Error::last_error(code)));
        }
        Ok((!filters.is_null()).then_some(Self(filters)))
    }
}

impl Drop for FilterList {
    fn drop(&mut self) {
        // SAFETY: `FilterList::load` is the sole constructor and transfers one
        // owned libgit2 filter-list handle into this wrapper.
        unsafe { git_filter_list_free(self.0) };
    }
}

#[repr(C)]
struct FilterSink {
    stream: raw::git_writestream,
    file: *mut File,
    error: Option<io::Error>,
}

impl FilterSink {
    fn new(file: &mut File) -> Self {
        Self {
            stream: raw::git_writestream {
                write: Some(filter_sink_write),
                close: Some(filter_sink_close),
                free: Some(filter_sink_free),
            },
            file,
            error: None,
        }
    }
}

extern "C" fn filter_sink_write(
    stream: *mut raw::git_writestream,
    buffer: *const c_char,
    length: usize,
) -> c_int {
    // SAFETY: libgit2 receives a pointer to the first field of a live
    // `FilterSink`, and calls this callback only before the streaming function
    // returns. A nonzero length always carries a valid input buffer.
    let sink = unsafe { &mut *stream.cast::<FilterSink>() };
    if sink.error.is_some() {
        return -1;
    }
    let bytes = if length == 0 {
        &[]
    } else {
        // SAFETY: guaranteed by the libgit2 writestream callback contract.
        unsafe { slice::from_raw_parts(buffer.cast::<u8>(), length) }
    };
    // SAFETY: `FilterSink::new` stores a live, exclusively borrowed file for
    // the complete synchronous filtering call.
    match unsafe { &mut *sink.file }.write_all(bytes) {
        Ok(()) => 0,
        Err(error) => {
            sink.error = Some(error);
            -1
        }
    }
}

extern "C" fn filter_sink_close(stream: *mut raw::git_writestream) -> c_int {
    // SAFETY: the same lifetime and layout guarantees as `filter_sink_write`
    // apply to the close callback.
    let sink = unsafe { &mut *stream.cast::<FilterSink>() };
    if sink.error.is_some() {
        return -1;
    }
    // SAFETY: the file pointer remains exclusively valid until filtering
    // returns.
    match unsafe { &mut *sink.file }.flush() {
        Ok(()) => 0,
        Err(error) => {
            sink.error = Some(error);
            -1
        }
    }
}

extern "C" fn filter_sink_free(_stream: *mut raw::git_writestream) {}

fn filter_error(path: &Path, source: git2::Error) -> GitError {
    if source.code() == ErrorCode::NotFound {
        stale(path)
    } else {
        inspection(path, source)
    }
}

const GIT_FILTER_TO_ODB: c_int = 1;
const GIT_FILTER_ALLOW_UNSAFE: u32 = 1 << 0;

unsafe extern "C" {
    fn git_filter_list_load(
        filters: *mut *mut RawFilterList,
        repository: *mut raw::git_repository,
        blob: *mut raw::git_blob,
        path: *const c_char,
        mode: c_int,
        flags: u32,
    ) -> c_int;
    fn git_filter_list_stream_file(
        filters: *mut RawFilterList,
        repository: *mut raw::git_repository,
        path: *const c_char,
        target: *mut raw::git_writestream,
    ) -> c_int;
    fn git_filter_list_free(filters: *mut RawFilterList);
}

fn read_bounded(path: &Path, absolute: &Path, limit: u64) -> Result<Vec<u8>, GitError> {
    let file = File::open(absolute).map_err(|source| io_or_stale(path, source))?;
    let mut content = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut content)
        .map_err(|source| io_or_stale(path, source))?;
    Ok(content)
}

fn refuse_stale_file(
    repository: &Repository,
    path: &Path,
    absolute: &Path,
    expected: Oid,
) -> Result<(), GitError> {
    let actual = Oid::hash_file_ext(ObjectType::Blob, absolute, repository.object_format())
        .map_err(|source| {
            if source.code() == ErrorCode::NotFound {
                stale(path)
            } else {
                inspection(path, source)
            }
        })?;
    refuse_stale(path, expected, actual)
}

fn refuse_stale_bytes(
    repository: &Repository,
    path: &Path,
    expected: Oid,
    content: &[u8],
) -> Result<(), GitError> {
    let actual = Oid::hash_object_ext(ObjectType::Blob, content, repository.object_format())
        .map_err(|source| inspection(path, source))?;
    refuse_stale(path, expected, actual)
}

fn refuse_stale(path: &Path, expected: Oid, actual: Oid) -> Result<(), GitError> {
    if expected == actual {
        Ok(())
    } else {
        Err(stale(path))
    }
}

fn project_lines(
    request: &FileContextRequest,
    id: Oid,
    content: &[u8],
) -> Result<FileContextResponse, GitError> {
    let byte_size = content.len() as u64;
    if matches!(request.range, FileContextRange::FullFile) && byte_size > request.max_total_bytes {
        return Ok(omitted(
            request,
            id,
            byte_size,
            FileContextOmission::ContentBudgetExhausted {
                limit: request.max_total_bytes,
            },
        ));
    }

    let (first, last) = requested_bounds(&request.range);
    let mut lines = Vec::new();
    let mut selected_bytes = 0u64;
    let mut budget_exhausted = false;
    let mut total_lines = 0u64;
    for raw_line in content.split_inclusive(|byte| *byte == b'\n') {
        total_lines = total_lines.saturating_add(1);
        if total_lines < first || total_lines > last || budget_exhausted {
            continue;
        }
        selected_bytes = selected_bytes.saturating_add(raw_line.len() as u64);
        if selected_bytes > request.max_total_bytes {
            budget_exhausted = true;
            lines.clear();
            continue;
        }
        let line_number = u32::try_from(total_lines).map_err(|_| GitError::MalformedDiff {
            detail:
                "file context contains more lines than the diff line-number model can represent"
                    .to_owned(),
        })?;
        let (old_line_number, new_line_number) = match request.side {
            FileSide::Old => (Some(line_number), None),
            FileSide::New => (None, Some(line_number)),
        };
        lines.push(DiffLine {
            kind: DiffLineKind::Context,
            old_line_number,
            new_line_number,
            content: raw_line.to_vec(),
            paired_line_index: None,
            intra_line_ranges: None,
        });
    }

    let total_lines = u32::try_from(total_lines).map_err(|_| GitError::MalformedDiff {
        detail: "file context contains more lines than the diff line-number model can represent"
            .to_owned(),
    })?;
    if budget_exhausted {
        return Ok(FileContextResponse {
            blob_id: id.to_string(),
            side: request.side,
            range: request.range.clone(),
            byte_size,
            total_lines: Some(total_lines),
            start_line: None,
            lines,
            omission: Some(FileContextOmission::ContentBudgetExhausted {
                limit: request.max_total_bytes,
            }),
        });
    }

    let start_line = lines.first().and_then(|line| match request.side {
        FileSide::Old => line.old_line_number,
        FileSide::New => line.new_line_number,
    });
    Ok(FileContextResponse {
        blob_id: id.to_string(),
        side: request.side,
        range: request.range.clone(),
        byte_size,
        total_lines: Some(total_lines),
        start_line,
        lines,
        omission: None,
    })
}

/// Inclusive one-based bounds for a request. A zero-width hunk side marks a
/// boundary between lines, so its preceding and following ranges do not count
/// the boundary itself as a line.
fn requested_bounds(range: &FileContextRange) -> (u64, u64) {
    match range {
        FileContextRange::FullFile => (1, u64::MAX),
        FileContextRange::Hunk {
            start_line,
            line_count,
            lines_before,
            lines_after,
        } if *line_count == 0 => {
            let start = u64::from(*start_line)
                .saturating_add(1)
                .saturating_sub(u64::from(*lines_before))
                .max(1);
            let end = u64::from(*start_line).saturating_add(u64::from(*lines_after));
            (start, end)
        }
        FileContextRange::Hunk {
            start_line,
            line_count,
            lines_before,
            lines_after,
        } => {
            let start = u64::from(*start_line)
                .saturating_sub(u64::from(*lines_before))
                .max(1);
            let end = u64::from(*start_line)
                .saturating_add(u64::from(*line_count).saturating_sub(1))
                .saturating_add(u64::from(*lines_after));
            (start, end)
        }
    }
}

fn omitted(
    request: &FileContextRequest,
    id: Oid,
    byte_size: u64,
    omission: FileContextOmission,
) -> FileContextResponse {
    FileContextResponse {
        blob_id: id.to_string(),
        side: request.side,
        range: request.range.clone(),
        byte_size,
        total_lines: None,
        start_line: None,
        lines: Vec::new(),
        omission: Some(omission),
    }
}

fn blob_lookup_error(root: &Path, id: Oid, source: git2::Error) -> GitError {
    if source.code() == ErrorCode::NotFound {
        GitError::BlobNotFound {
            blob_id: id.to_string(),
        }
    } else {
        inspection(root, source)
    }
}

fn io_or_stale(path: &Path, source: io::Error) -> GitError {
    if source.kind() == io::ErrorKind::NotFound {
        stale(path)
    } else {
        GitError::DiffContent {
            path: path.to_path_buf(),
            source,
        }
    }
}

fn stale(path: &Path) -> GitError {
    GitError::StaleHunkSelection {
        path: path.to_path_buf(),
    }
}

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_str_bytes(value: &OsStr) -> &[u8] {
    value.to_str().unwrap_or_default().as_bytes()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use git2::Repository;

    use super::{FileContextOmission, FileContextRequest, FileSide};
    use crate::{
        git::{DiffLineKind, DiffOptions, DiffTarget, GitError, GitService},
        testing::{Fixture, commit_all, initialize_repository},
    };

    #[test]
    fn hunk_expansion_matches_the_same_hunk_with_wider_git_context() {
        let fixture = Fixture::new();
        let root = fixture.directory("expanded-context");
        let repository = initialize_repository(&root);
        let old = numbered_lines(20, None);
        let new = numbered_lines(20, Some((10, "changed ten")));
        fs::write(root.join("tracked.txt"), &old).unwrap();
        commit_all(&repository, "add expansion fixture");
        fs::write(root.join("tracked.txt"), &new).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = &files[0];
        let hunk = &file.hunks[0];

        let wider = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_context_lines(5),
            )
            .unwrap();
        let wider_hunk = &wider[0].hunks[0];
        for side in [FileSide::Old, FileSide::New] {
            let response = service
                .file_context(&FileContextRequest::for_hunk(file, hunk, side, 2, 2))
                .unwrap();
            let expected = wider_hunk
                .lines
                .iter()
                .filter(|line| match side {
                    FileSide::Old => line.kind != DiffLineKind::Addition,
                    FileSide::New => line.kind != DiffLineKind::Deletion,
                })
                .filter(|line| {
                    line.kind == DiffLineKind::Context
                        || match side {
                            FileSide::Old => line.kind == DiffLineKind::Deletion,
                            FileSide::New => line.kind == DiffLineKind::Addition,
                        }
                })
                .map(|line| {
                    (
                        match side {
                            FileSide::Old => line.old_line_number,
                            FileSide::New => line.new_line_number,
                        },
                        line.content.clone(),
                    )
                })
                .collect::<Vec<_>>();
            let actual = response
                .lines
                .iter()
                .map(|line| {
                    (
                        match side {
                            FileSide::Old => line.old_line_number,
                            FileSide::New => line.new_line_number,
                        },
                        line.content.clone(),
                    )
                })
                .collect::<Vec<_>>();

            assert_eq!(actual, expected, "wrong {side:?} expansion");
            assert_eq!(response.omission, None);
        }
        assert!(!fixture.data_dir.exists(), "context retrieval took a lock");
    }

    #[test]
    fn zero_width_hunk_side_expands_around_its_line_boundary() {
        let fixture = Fixture::new();
        let root = fixture.directory("zero-width-context");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"one\ntwo\nthree\nfour\n").unwrap();
        commit_all(&repository, "add insertion fixture");
        fs::write(
            root.join("tracked.txt"),
            b"one\ntwo\ninserted\nthree\nfour\n",
        )
        .unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_context_lines(0),
            )
            .unwrap();
        let request =
            FileContextRequest::for_hunk(&files[0], &files[0].hunks[0], FileSide::Old, 1, 1);

        let response = service.file_context(&request).unwrap();

        assert_eq!(
            response
                .lines
                .iter()
                .map(|line| line.content.as_slice())
                .collect::<Vec<_>>(),
            vec![b"two\n".as_slice(), b"three\n".as_slice()]
        );
        assert_eq!(response.start_line, Some(2));
    }

    #[test]
    fn staged_context_stays_blob_stable_after_a_further_worktree_edit() {
        let fixture = Fixture::new();
        let root = fixture.directory("stable-staged-context");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        commit_all(&repository, "add stable fixture");
        fs::write(root.join("tracked.txt"), b"staged bytes\n").unwrap();
        stage(&repository, Path::new("tracked.txt"));
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let request = FileContextRequest::full_file(&files[0], FileSide::New);

        fs::write(root.join("tracked.txt"), b"later worktree bytes\n").unwrap();
        let response = service.file_context(&request).unwrap();

        assert_eq!(joined(&response.lines), b"staged bytes\n");
        assert_eq!(response.blob_id, files[0].new_blob_id);
    }

    #[test]
    fn changed_unstaged_context_is_a_stale_hunk_refusal() {
        let fixture = Fixture::new();
        let root = fixture.directory("stale-unstaged-context");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        commit_all(&repository, "add stale fixture");
        fs::write(root.join("tracked.txt"), b"first edit\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        // A size summary must not mask staleness when the source has changed.
        let request = FileContextRequest::full_file(&files[0], FileSide::New).with_max_file_size(5);

        fs::write(root.join("tracked.txt"), b"second edit\n").unwrap();

        assert!(matches!(
            service.file_context(&request),
            Err(GitError::StaleHunkSelection { path }) if path == Path::new("tracked.txt")
        ));
    }

    #[test]
    fn absent_addition_and_deletion_sides_return_empty_files() {
        let fixture = Fixture::new();
        let root = fixture.directory("absent-context-sides");
        let repository = initialize_repository(&root);
        fs::write(root.join("added.txt"), b"added\n").unwrap();
        stage(&repository, Path::new("added.txt"));
        fs::remove_file(root.join("tracked.txt")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let addition = staged
            .iter()
            .find(|file| file.new_path.as_deref() == Some(Path::new("added.txt")))
            .unwrap();
        let deletion = unstaged
            .iter()
            .find(|file| file.old_path.as_deref() == Some(Path::new("tracked.txt")))
            .unwrap();

        for request in [
            FileContextRequest::full_file(addition, FileSide::Old),
            FileContextRequest::full_file(deletion, FileSide::New),
        ] {
            let response = service.file_context(&request).unwrap();
            assert_eq!(response.byte_size, 0);
            assert_eq!(response.total_lines, Some(0));
            assert_eq!(response.start_line, None);
            assert!(response.lines.is_empty());
            assert_eq!(response.omission, None);
            assert!(response.blob_id.chars().all(|byte| byte == '0'));
        }
    }

    #[cfg(unix)]
    #[test]
    fn worktree_symlink_context_reads_the_link_instead_of_following_it() {
        use std::{os::unix::ffi::OsStrExt, os::unix::fs::symlink};

        let fixture = Fixture::new();
        let root = fixture.directory("symlink-context");
        let outside = fixture.directory("symlink-targets");
        let repository = initialize_repository(&root);
        let original_target = outside.join("original");
        let changed_target = outside.join("changed");
        fs::write(&original_target, b"outside original\n").unwrap();
        fs::write(&changed_target, b"outside changed\n").unwrap();
        symlink(&original_target, root.join("link")).unwrap();
        commit_all(&repository, "add external symlink");
        fs::remove_file(root.join("link")).unwrap();
        symlink(&changed_target, root.join("link")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = files
            .iter()
            .find(|file| file.new_path.as_deref() == Some(Path::new("link")))
            .unwrap();

        let response = service
            .file_context(&FileContextRequest::full_file(file, FileSide::New))
            .unwrap();

        assert_eq!(
            joined(&response.lines),
            changed_target.as_os_str().as_bytes()
        );

        let outside_request = FileContextRequest::worktree(
            &changed_target,
            git2::Oid::hash_object_ext(
                git2::ObjectType::Blob,
                b"outside changed\n",
                repository.object_format(),
            )
            .unwrap()
            .to_string(),
            FileSide::New,
            super::FileContextRange::FullFile,
        );
        assert!(matches!(
            service.file_context(&outside_request),
            Err(GitError::PathOutsideRepository { path, .. }) if path == changed_target
        ));
    }

    #[test]
    fn worktree_context_applies_the_same_clean_filter_as_the_diff() {
        let fixture = Fixture::new();
        let root = fixture.directory("autocrlf-context");
        let repository = initialize_repository(&root);
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", true)
            .unwrap();
        fs::write(root.join(".gitignore"), b"crlf.txt\n").unwrap();
        fs::write(root.join("crlf.txt"), b"base\r\n").unwrap();
        stage(&repository, Path::new("crlf.txt"));
        commit_all(&repository, "add crlf fixture");
        repository
            .config()
            .unwrap()
            .set_bool("core.safecrlf", true)
            .unwrap();
        let worktree = b"changed\r\nmixed\n";
        fs::write(root.join("crlf.txt"), worktree).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = files
            .iter()
            .find(|file| file.new_path.as_deref() == Some(Path::new("crlf.txt")))
            .unwrap();
        assert_eq!(file.new_size, worktree.len() as u64);
        let diff_new = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind != DiffLineKind::Deletion)
            .flat_map(|line| line.content.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(diff_new, b"changed\nmixed\n");
        assert_eq!(
            file.new_blob_id,
            git2::Oid::hash_object_ext(
                git2::ObjectType::Blob,
                &diff_new,
                repository.object_format(),
            )
            .unwrap()
            .to_string()
        );

        let request = FileContextRequest::full_file(file, FileSide::New);
        let response = service.file_context(&request).unwrap();

        assert_eq!(joined(&response.lines), diff_new);
        assert_eq!(response.blob_id, file.new_blob_id);

        let limited = service
            .file_context(&request.clone().with_max_file_size(1))
            .unwrap();
        assert_eq!(
            limited.omission,
            Some(FileContextOmission::FileTooLarge { limit: 1 })
        );
        assert_eq!(limited.byte_size, diff_new.len() as u64);

        fs::write(root.join("crlf.txt"), b"changed again\r\n").unwrap();
        assert!(matches!(
            service.file_context(&request),
            Err(GitError::StaleHunkSelection { path }) if path == Path::new("crlf.txt")
        ));
    }

    #[test]
    fn omitted_filtered_diff_retains_its_recorded_raw_worktree_identity() {
        let fixture = Fixture::new();
        let root = fixture.directory("omitted-autocrlf-context");
        let repository = initialize_repository(&root);
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", true)
            .unwrap();
        fs::write(root.join("crlf.txt"), b"base\r\n").unwrap();
        commit_all(&repository, "add omitted crlf fixture");
        let content = b"changed\r\n";
        fs::write(root.join("crlf.txt"), content).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_max_file_size(1),
            )
            .unwrap();
        let file = files
            .iter()
            .find(|file| file.new_path.as_deref() == Some(Path::new("crlf.txt")))
            .unwrap();
        let raw_id =
            git2::Oid::hash_object_ext(git2::ObjectType::Blob, content, repository.object_format())
                .unwrap()
                .to_string();
        assert_eq!(file.new_blob_id, raw_id);

        let response = service
            .file_context(&FileContextRequest::full_file(file, FileSide::New))
            .unwrap();
        assert_eq!(joined(&response.lines), content);

        let limited = service
            .file_context(&FileContextRequest::full_file(file, FileSide::New).with_max_file_size(1))
            .unwrap();
        assert_eq!(
            limited.omission,
            Some(FileContextOmission::FileTooLarge { limit: 1 })
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_file_worktree_sources_are_refused_without_reading_them() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let fixture = Fixture::new();
        let root = fixture.directory("special-file-context");
        initialize_repository(&root);
        let pipe = root.join("pipe");
        let pipe_string = CString::new(pipe.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid, NUL-terminated temporary path and the
        // fixture owns its parent directory.
        assert_eq!(unsafe { libc::mkfifo(pipe_string.as_ptr(), 0o600) }, 0);
        let service = GitService::new(&root, &fixture.data_dir);
        let request = FileContextRequest::worktree(
            Path::new("pipe"),
            git2::Oid::ZERO_SHA1.to_string(),
            FileSide::New,
            super::FileContextRange::FullFile,
        );

        assert!(matches!(
            service.file_context(&request),
            Err(GitError::StaleHunkSelection { path }) if path == Path::new("pipe")
        ));
    }

    #[test]
    fn full_non_utf8_file_round_trips_byte_for_byte() {
        let fixture = Fixture::new();
        let root = fixture.directory("non-utf8-context");
        let repository = initialize_repository(&root);
        fs::write(root.join("bytes.txt"), b"old\n").unwrap();
        commit_all(&repository, "add byte fixture");
        let content = b"first\n\xffsecond\nthird\xfe\0";
        fs::write(root.join("bytes.txt"), content).unwrap();
        stage(&repository, Path::new("bytes.txt"));
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        assert!(
            files[0].binary,
            "the full-file API must also serve binary bytes"
        );

        let response = service
            .file_context(&FileContextRequest::full_file(&files[0], FileSide::New))
            .unwrap();

        assert_eq!(joined(&response.lines), content);
        assert_eq!(response.total_lines, Some(3));
    }

    #[test]
    fn both_context_limits_return_named_untruncated_summaries() {
        let fixture = Fixture::new();
        let root = fixture.directory("bounded-context");
        let repository = initialize_repository(&root);
        let content = b"one line\ntwo lines\nthree lines\n";
        fs::write(root.join("bounded.txt"), content).unwrap();
        commit_all(&repository, "add bounded fixture");
        let id = repository
            .head()
            .unwrap()
            .peel_to_tree()
            .unwrap()
            .get_path(Path::new("bounded.txt"))
            .unwrap()
            .id();
        let service = GitService::new(&root, &fixture.data_dir);

        let by_file = service
            .file_context(
                &FileContextRequest::full_blob(id.to_string(), FileSide::New)
                    .with_max_file_size(10),
            )
            .unwrap();
        assert_eq!(
            by_file.omission,
            Some(FileContextOmission::FileTooLarge { limit: 10 })
        );
        assert!(by_file.lines.is_empty());
        assert_eq!(by_file.byte_size, content.len() as u64);
        assert_eq!(by_file.total_lines, None);

        let by_content = service
            .file_context(
                &FileContextRequest::full_blob(id.to_string(), FileSide::New)
                    .with_max_total_bytes(10),
            )
            .unwrap();
        assert_eq!(
            by_content.omission,
            Some(FileContextOmission::ContentBudgetExhausted { limit: 10 })
        );
        assert!(by_content.lines.is_empty());
    }

    #[test]
    fn absent_and_malformed_blob_ids_are_distinct_typed_errors() {
        let fixture = Fixture::new();
        let root = fixture.directory("missing-context-blob");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);

        assert!(matches!(
            service.file_context(&FileContextRequest::full_blob(
                "1".repeat(40),
                FileSide::New,
            )),
            Err(GitError::BlobNotFound { blob_id }) if blob_id == "1".repeat(40)
        ));
        assert!(matches!(
            service.file_context(&FileContextRequest::full_blob(
                "not-an-object-id",
                FileSide::New,
            )),
            Err(GitError::InvalidBlobId { blob_id }) if blob_id == "not-an-object-id"
        ));
    }

    fn numbered_lines(count: usize, replacement: Option<(usize, &str)>) -> Vec<u8> {
        (1..=count)
            .map(|number| match replacement {
                Some((line, text)) if number == line => format!("{text}\n"),
                _ => format!("line {number}\n"),
            })
            .collect::<String>()
            .into_bytes()
    }

    fn joined(lines: &[crate::git::DiffLine]) -> Vec<u8> {
        lines
            .iter()
            .flat_map(|line| line.content.iter().copied())
            .collect()
    }

    fn stage(repository: &Repository, path: &Path) {
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
    }
}
