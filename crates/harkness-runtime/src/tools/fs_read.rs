//! Bounded, byte-preserving reads of one contained workspace file.

use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::{ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata};

/// Default maximum number of file bytes returned inline.
pub const DEFAULT_FS_READ_MAX_BYTES: u64 = 32 * 1024;
/// Absolute maximum one `fs.read` invocation may return.
pub const MAX_FS_READ_BYTES: u64 = 32 * 1024;

/// Input to `fs.read@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FsReadInput {
    /// Workspace-relative or contained absolute path to a regular file.
    pub path: String,
    /// Zero-based line offset. Defaults to the first line.
    pub offset: Option<u64>,
    /// Maximum number of lines to return after `offset`.
    #[schemars(range(min = 1, max = 1_000_000))]
    pub limit: Option<u64>,
    /// Maximum returned content bytes. Defaults to 32 KiB, leaving room for
    /// Base64 expansion and metadata inside the runtime's inline record bound.
    #[schemars(range(min = 1, max = 32768))]
    pub max_bytes: Option<u64>,
}

/// Encoding used by a byte-preserving content field.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentEncoding {
    /// Content was valid UTF-8 and is carried directly.
    Utf8,
    /// Content is Base64 over the exact returned bytes.
    Base64,
}

/// Why a file read returned only a prefix of the requested range.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReadTruncation {
    /// The hard returned-byte cap was reached.
    ByteLimit {
        /// Cap applied to the returned bytes.
        limit: u64,
    },
    /// The caller's requested line count was reached.
    LineLimit {
        /// Line count applied after the offset.
        limit: u64,
    },
}

/// Result of `fs.read@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsReadOutput {
    /// Path exactly as supplied by the caller.
    pub path: String,
    /// Selected bytes, either direct UTF-8 or Base64.
    pub content: String,
    /// How to decode `content` back to bytes.
    pub content_encoding: ContentEncoding,
    /// Basic content classification derived from the returned bytes.
    pub media_type: String,
    /// Total size of the file, not merely the returned range.
    pub byte_size: u64,
    /// Number of decoded bytes carried in `content`.
    pub returned_bytes: u64,
    /// Zero-based line offset that was applied.
    pub offset: u64,
    /// Named reason more requested bytes remain.
    pub truncated: Option<ReadTruncation>,
    /// Whether any executable bit is set on platforms that expose Unix modes.
    pub executable: bool,
}

/// The production `fs.read@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsRead;

impl Tool for FsRead {
    type Input = FsReadInput;
    type Output = FsReadOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fs.read", "1.0.0").expect("a built-in tool identity"),
            "Read a workspace file",
            "Reads a contained regular file by line range with a hard byte cap and byte-preserving UTF-8 or Base64 output.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        context.check_still_permitted()?;
        let path = context.resolve(Path::new(&input.path))?;
        let metadata = match fs::metadata(path.as_path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::NotFound {
                    path: Path::new(&input.path).to_path_buf(),
                });
            }
            Err(error) => return Err(ToolError::execution_failed(error)),
        };
        if !metadata.is_file() {
            return Err(ToolError::execution_failed(format!(
                "{} is not a regular file",
                input.path
            )));
        }
        let max_bytes = input.max_bytes.unwrap_or(DEFAULT_FS_READ_MAX_BYTES);
        debug_assert!(max_bytes <= MAX_FS_READ_BYTES);
        let offset = input.offset.unwrap_or(0);
        let (bytes, truncated) = read_lines(
            path.as_path(),
            metadata.len(),
            offset,
            input.limit,
            max_bytes,
            context,
        )?;
        let binary = bytes.contains(&0);
        let (content, content_encoding) = match String::from_utf8(bytes.clone()) {
            Ok(text) => (text, ContentEncoding::Utf8),
            Err(_) => (BASE64.encode(&bytes), ContentEncoding::Base64),
        };
        Ok(FsReadOutput {
            path: input.path,
            content,
            content_encoding,
            media_type: if binary || matches!(content_encoding, ContentEncoding::Base64) {
                "application/octet-stream".to_owned()
            } else {
                "text/plain".to_owned()
            },
            byte_size: metadata.len(),
            returned_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            offset,
            truncated,
            executable: executable(&metadata),
        })
    }
}

fn read_lines(
    path: &Path,
    byte_size: u64,
    offset: u64,
    limit: Option<u64>,
    max_bytes: u64,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, Option<ReadTruncation>), ToolError> {
    let mut file = File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ToolError::NotFound {
            path: path.to_path_buf(),
        },
        _ => ToolError::execution_failed(error),
    })?;
    let capacity = usize::try_from(max_bytes.min(64 * 1024)).unwrap_or(64 * 1024);
    let mut selected = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; 8192];
    let mut line = 0_u64;
    let end_line = limit.map(|count| offset.saturating_add(count));
    let mut consumed = 0_u64;

    loop {
        context.check_still_permitted()?;
        let read = file
            .read(&mut buffer)
            .map_err(ToolError::execution_failed)?;
        if read == 0 {
            return Ok((selected, None));
        }
        for byte in &buffer[..read] {
            consumed = consumed.saturating_add(1);
            if line >= offset && end_line.is_none_or(|end| line < end) {
                if u64::try_from(selected.len()).unwrap_or(u64::MAX) == max_bytes {
                    return Ok((
                        selected,
                        Some(ReadTruncation::ByteLimit { limit: max_bytes }),
                    ));
                }
                selected.push(*byte);
            }
            if *byte == b'\n' {
                line = line.saturating_add(1);
                if let Some(end) = end_line
                    && line >= end
                    && consumed < byte_size
                {
                    return Ok((
                        selected,
                        Some(ReadTruncation::LineLimit {
                            limit: limit.expect("an end line came from a limit"),
                        }),
                    ));
                }
            }
        }
    }
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}
