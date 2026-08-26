//! Explicit, bounded line attribution through porcelain `git blame`.
//!
//! Blame is intentionally unlike the other read surfaces in this crate. It is
//! expensive, easy to invoke accidentally, and has no byte-preserving libgit2
//! equivalent in the workspace, so callers must supply a line range and the
//! operation goes through the hermetic system-Git runner. Nothing else calls
//! it implicitly.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::runner::{Cancellation, GitAccess, GitCommand};
use crate::{GitError, commit};

/// The most source lines one blame request may attribute.
pub const MAX_BLAME_LINES: u32 = 10_000;

/// The most porcelain output retained for one blame request.
pub const MAX_BLAME_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// One inclusive, one-based source line range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlameLineRange {
    start: u32,
    end: u32,
}

impl BlameLineRange {
    /// Validates and builds an inclusive range.
    pub fn new(start: u32, end: u32) -> Result<Self, GitError> {
        if start == 0 || end < start {
            return Err(GitError::InvalidBlameRange { start, end });
        }
        let lines = end - start + 1;
        if lines > MAX_BLAME_LINES {
            return Err(GitError::BlameRangeTooLarge {
                lines,
                limit: MAX_BLAME_LINES,
            });
        }
        Ok(Self { start, end })
    }

    /// First one-based line, inclusive.
    #[must_use]
    pub const fn start(self) -> u32 {
        self.start
    }

    /// Last one-based line, inclusive.
    #[must_use]
    pub const fn end(self) -> u32 {
        self.end
    }

    /// Number of lines in the range.
    #[must_use]
    pub const fn line_count(self) -> u32 {
        self.end - self.start + 1
    }
}

/// The attribution Git recorded for one run of lines.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlameCommit {
    /// An immutable commit object.
    Commit(String),
    /// A line whose working-tree form is not in a commit.
    Uncommitted,
}

/// One consecutive run with the same attribution.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct BlameEntry {
    /// Commit attribution, with dirty lines named rather than fabricated.
    pub commit: BlameCommit,
    /// Path recorded on the attributed commit side, retained byte-exactly on Unix.
    pub original_path: PathBuf,
    /// First one-based line in the attributed commit.
    pub original_start_line: u32,
    /// First one-based line in the requested working-tree file.
    pub final_start_line: u32,
    /// Number of consecutive lines represented by this entry.
    pub line_count: u32,
    /// Author timestamp from the commit, in Unix epoch seconds.
    pub author_time: Option<i64>,
}

/// Attribution for one explicit file range.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileBlame {
    /// Repository-relative path that was requested.
    pub path: PathBuf,
    /// Inclusive range that was requested.
    pub range: BlameLineRange,
    /// Consecutive attribution runs in final-line order.
    pub entries: Vec<BlameEntry>,
}

pub(crate) fn file(
    git_executable: &Path,
    root: &Path,
    path: &Path,
    range: BlameLineRange,
    cancellation: &Cancellation,
) -> Result<FileBlame, GitError> {
    commit::validate_paths(root, &[path.to_path_buf()])?;
    let line_range = format!("{},{}", range.start, range.end);
    let output = GitCommand::new(git_executable, root, GitAccess::LocalRead)
        .args(["blame", "--porcelain", "--root", "--no-progress", "-L"])
        .arg(&line_range)
        .arg("--")
        .arg(path)
        .with_max_stdout_bytes(MAX_BLAME_OUTPUT_BYTES)
        .run(cancellation)?;
    parse(path, range, &output.stdout)
}

#[derive(Clone)]
struct CommitMetadata {
    original_path: PathBuf,
    author_time: Option<i64>,
}

struct PendingLine {
    id: Vec<u8>,
    original_line: u32,
    final_line: u32,
    original_path: Option<PathBuf>,
    author_time: Option<i64>,
}

fn parse(path: &Path, range: BlameLineRange, bytes: &[u8]) -> Result<FileBlame, GitError> {
    let mut metadata = HashMap::<Vec<u8>, CommitMetadata>::new();
    let mut pending = None::<PendingLine>;
    let mut entries = Vec::<BlameEntry>::new();
    let mut attributed_lines = 0_u32;

    for raw_line in bytes.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"\t") {
            let current = pending
                .take()
                .ok_or_else(|| malformed("content had no blame header"))?;
            let cached = metadata.get(&current.id);
            let original_path = current
                .original_path
                .or_else(|| cached.map(|value| value.original_path.clone()))
                .unwrap_or_else(|| path.to_path_buf());
            let author_time = current
                .author_time
                .or_else(|| cached.and_then(|value| value.author_time));
            metadata.insert(
                current.id.clone(),
                CommitMetadata {
                    original_path: original_path.clone(),
                    author_time,
                },
            );
            let commit = if current.id.iter().all(|byte| *byte == b'0') {
                BlameCommit::Uncommitted
            } else {
                BlameCommit::Commit(
                    String::from_utf8(current.id)
                        .map_err(|_| malformed("object id was not ASCII"))?,
                )
            };
            push_entry(
                &mut entries,
                BlameEntry {
                    commit,
                    original_path,
                    original_start_line: current.original_line,
                    final_start_line: current.final_line,
                    line_count: 1,
                    author_time,
                },
            );
            attributed_lines = attributed_lines
                .checked_add(1)
                .ok_or_else(|| malformed("attributed line count overflowed"))?;
            continue;
        }

        if let Some(current) = pending.as_mut() {
            if let Some(value) = line.strip_prefix(b"author-time ") {
                current.author_time = Some(parse_i64(value, "author time")?);
            } else if let Some(value) = line.strip_prefix(b"filename ") {
                current.original_path = Some(parse_path(value)?);
            }
            continue;
        }

        pending = Some(parse_header(line)?);
    }

    if pending.is_some() {
        return Err(malformed("blame record ended before its content line"));
    }
    if attributed_lines != range.line_count() {
        return Err(malformed(format!(
            "range contains {} lines but Git returned {attributed_lines}",
            range.line_count()
        )));
    }
    Ok(FileBlame {
        path: path.to_path_buf(),
        range,
        entries,
    })
}

fn parse_header(line: &[u8]) -> Result<PendingLine, GitError> {
    let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
    if fields.len() < 3 || !valid_object_id(fields[0]) {
        return Err(malformed(
            "record header did not name an object and two line numbers",
        ));
    }
    Ok(PendingLine {
        id: fields[0].to_vec(),
        original_line: parse_u32(fields[1], "original line")?,
        final_line: parse_u32(fields[2], "final line")?,
        original_path: None,
        author_time: None,
    })
}

fn valid_object_id(value: &[u8]) -> bool {
    matches!(value.len(), 40 | 64) && value.iter().all(u8::is_ascii_hexdigit)
}

fn parse_u32(value: &[u8], field: &str) -> Result<u32, GitError> {
    let value =
        std::str::from_utf8(value).map_err(|_| malformed(format!("{field} was not ASCII")))?;
    value
        .parse()
        .map_err(|_| malformed(format!("{field} was not an unsigned integer")))
}

fn parse_i64(value: &[u8], field: &str) -> Result<i64, GitError> {
    let value =
        std::str::from_utf8(value).map_err(|_| malformed(format!("{field} was not ASCII")))?;
    value
        .parse()
        .map_err(|_| malformed(format!("{field} was not an integer")))
}

fn push_entry(entries: &mut Vec<BlameEntry>, next: BlameEntry) {
    if let Some(previous) = entries.last_mut()
        && previous.commit == next.commit
        && previous.original_path == next.original_path
        && previous.author_time == next.author_time
        && previous.original_start_line + previous.line_count == next.original_start_line
        && previous.final_start_line + previous.line_count == next.final_start_line
    {
        previous.line_count += 1;
    } else {
        entries.push(next);
    }
}

fn parse_path(value: &[u8]) -> Result<PathBuf, GitError> {
    let bytes = if value.starts_with(b"\"") {
        if !value.ends_with(b"\"") || value.len() < 2 {
            return Err(malformed("quoted filename was not terminated"));
        }
        unquote(&value[1..value.len() - 1])?
    } else {
        value.to_vec()
    };
    Ok(PathBuf::from(bytes_to_os_string(bytes)))
}

fn unquote(value: &[u8]) -> Result<Vec<u8>, GitError> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'\\' {
            decoded.push(value[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = *value
            .get(index)
            .ok_or_else(|| malformed("filename ended inside an escape"))?;
        index += 1;
        match escaped {
            b'\\' | b'\"' => decoded.push(escaped),
            b'a' => decoded.push(0x07),
            b'b' => decoded.push(0x08),
            b't' => decoded.push(b'\t'),
            b'n' => decoded.push(b'\n'),
            b'v' => decoded.push(0x0b),
            b'f' => decoded.push(0x0c),
            b'r' => decoded.push(b'\r'),
            b'0'..=b'7' => {
                let mut number = u16::from(escaped - b'0');
                for _ in 0..2 {
                    let Some(next @ b'0'..=b'7') = value.get(index).copied() else {
                        break;
                    };
                    number = number * 8 + u16::from(next - b'0');
                    index += 1;
                }
                let byte = u8::try_from(number)
                    .map_err(|_| malformed("filename octal escape exceeded one byte"))?;
                decoded.push(byte);
            }
            _ => return Err(malformed("filename contained an unknown escape")),
        }
    }
    Ok(decoded)
}

#[cfg(unix)]
fn bytes_to_os_string(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn bytes_to_os_string(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

fn malformed(detail: impl Into<String>) -> GitError {
    GitError::MalformedBlame {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{BlameCommit, BlameLineRange, MAX_BLAME_LINES, parse};
    use crate::testing::{Fixture, commit_all, git, initialize_repository};
    use crate::{Cancellation, GitError, GitService};

    #[test]
    fn range_is_one_based_ordered_and_bounded() {
        assert!(matches!(
            BlameLineRange::new(0, 1),
            Err(GitError::InvalidBlameRange { .. })
        ));
        assert!(matches!(
            BlameLineRange::new(2, 1),
            Err(GitError::InvalidBlameRange { .. })
        ));
        assert!(matches!(
            BlameLineRange::new(1, MAX_BLAME_LINES + 1),
            Err(GitError::BlameRangeTooLarge { .. })
        ));
    }

    #[test]
    fn porcelain_projection_coalesces_runs_and_decodes_paths() {
        let range = BlameLineRange::new(2, 3).unwrap();
        let id = "1".repeat(40);
        let bytes = format!(
            "{id} 7 2 2\nauthor-time 123\nfilename \"old\\040name.txt\"\n\tfirst\n\
             {id} 8 3\n\tsecond\n"
        );
        let blame = parse(Path::new("new.txt"), range, bytes.as_bytes()).unwrap();
        assert_eq!(blame.entries.len(), 1);
        assert_eq!(blame.entries[0].original_path, Path::new("old name.txt"));
        assert_eq!(blame.entries[0].line_count, 2);
        assert_eq!(blame.entries[0].author_time, Some(123));
    }

    #[test]
    fn service_marks_worktree_only_lines_uncommitted() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("file.txt"), "committed\nsecond\n").unwrap();
        commit_all(&repository, "base");
        fs::write(root.join("file.txt"), "dirty\nsecond\n").unwrap();

        let blame = GitService::new(&root, &fixture.data_dir)
            .blame_file(
                "file.txt",
                BlameLineRange::new(1, 2).unwrap(),
                &Cancellation::default(),
            )
            .unwrap();
        assert!(
            blame
                .entries
                .iter()
                .any(|entry| entry.commit == BlameCommit::Uncommitted)
        );
        assert_eq!(git(&root, ["status", "--short"]), " M file.txt\n");
    }

    #[test]
    fn a_five_thousand_line_range_stays_inside_the_published_bound() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        let content = (1..=5_000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("large.txt"), content).unwrap();
        commit_all(&repository, "large blame fixture");

        let blame = GitService::new(&root, &fixture.data_dir)
            .blame_file(
                "large.txt",
                BlameLineRange::new(1, 5_000).unwrap(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            blame
                .entries
                .iter()
                .map(|entry| entry.line_count)
                .sum::<u32>(),
            5_000
        );
    }
}
