//! Bounded, blob-addressed file and hunk context.
//!
//! A diff records the object ID of each content side. Those IDs are the stable
//! address here: index and revision content is read from the immutable object
//! database even if a path changes later. The one exception is a working-tree
//! side, whose recorded ID is only a hash. That source is read by path and its
//! bytes are accepted only when they still hash to the recorded ID.
//!
//! Retrieval is local, read-only inspection. It uses libgit2 in process, takes
//! no repository lock and never spawns system Git.

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use git2::{ErrorCode, ObjectType, Oid, Repository};

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
    /// The one diff side that has a hash but no stored blob.
    ///
    /// `expected_blob_id` is the content hash recorded by the diff. The path is
    /// used only to read the current bytes, never as their identity.
    Worktree {
        path: PathBuf,
        expected_blob_id: String,
    },
}

impl FileContextSource {
    fn blob_id(&self) -> &str {
        match self {
            Self::Blob { blob_id } => blob_id,
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
        if side == FileSide::New
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
    /// Size of the complete source in bytes.
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
    if let FileContextSource::Worktree { path, .. } = &request.source {
        commit::validate_paths(root, std::slice::from_ref(path))?;
    }
    let repository = commit::open(root)?;
    let id = parse_blob_id(&repository, request.source.blob_id())?;

    match &request.source {
        FileContextSource::Blob { .. } => load_blob(&repository, root, id, request),
        FileContextSource::Worktree { path, .. } => {
            load_worktree(&repository, root, path, id, request)
        }
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

fn load_worktree(
    repository: &Repository,
    root: &Path,
    path: &Path,
    expected: Oid,
    request: &FileContextRequest,
) -> Result<FileContextResponse, GitError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let metadata = fs::symlink_metadata(&absolute).map_err(|source| io_or_stale(path, source))?;

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&absolute).map_err(|source| io_or_stale(path, source))?;
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

    // An oversized working-tree source still has to prove it is the content
    // the diff recorded. Hashing streams through libgit2 and does not place the
    // file in the response model.
    if metadata.len() > request.max_file_size {
        refuse_stale_file(repository, path, &absolute, expected)?;
        let current_size = fs::symlink_metadata(&absolute)
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

    let content = read_bounded(path, &absolute, request.max_file_size)?;
    if content.len() as u64 > request.max_file_size {
        // The file grew after the metadata check. Revalidate the complete file
        // before returning a size summary so changed content is still stale.
        refuse_stale_file(repository, path, &absolute, expected)?;
        let byte_size = fs::symlink_metadata(&absolute)
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
