//! Bounded in-process workspace search with Git ignore awareness.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::{
    ExecutionContext, RequestEffects, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata,
};
use crate::trust::{PathAccess, PathBoundary};

use super::git_status::{map_git_error, project_path};

/// Default global match count.
pub const DEFAULT_SEARCH_MAX_MATCHES: usize = 200;
/// Default matches retained from one file.
pub const DEFAULT_SEARCH_MAX_PER_FILE: usize = 20;
/// Default combined excerpt bytes.
pub const DEFAULT_SEARCH_TOTAL_BYTES: usize = 32 * 1024;
/// Hard regex source length.
pub const MAX_SEARCH_PATTERN_BYTES: usize = 1024;
/// Hard number of visited files.
pub const MAX_SEARCH_FILES: usize = 10_000;
/// Hard bytes inspected by one search.
pub const MAX_SEARCH_SCANNED_BYTES: u64 = 64 * 1024 * 1024;

/// Input to `workspace.search@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSearchInput {
    /// Literal or regex source to find.
    #[schemars(length(min = 1, max = 1024))]
    pub query: String,
    /// Search root within the workspace. Defaults to the workspace root.
    pub path: Option<String>,
    /// Interpret `query` as a regex. Defaults to literal matching.
    pub regex: Option<bool>,
    /// Use case-sensitive matching. Defaults to true.
    pub case_sensitive: Option<bool>,
    /// Maximum matches in the response.
    #[schemars(range(min = 1, max = 1000))]
    pub max_matches: Option<usize>,
    /// Maximum matches retained from one file.
    #[schemars(range(min = 1, max = 100))]
    pub max_per_file: Option<usize>,
    /// Combined excerpt-byte budget. Defaults to 32 KiB and cannot exceed 48 KiB.
    #[schemars(range(min = 1, max = 49152))]
    pub max_total_bytes: Option<usize>,
    /// Maximum bytes in one match excerpt.
    #[schemars(range(min = 16, max = 1024))]
    pub max_excerpt_bytes: Option<usize>,
}

/// One search match.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchMatch {
    pub path: String,
    pub path_is_lossy: bool,
    pub path_base64: Option<String>,
    /// One-based line number.
    pub line_number: u64,
    /// Zero-based UTF-8 byte column within the line.
    pub byte_column: u64,
    /// Bounded line excerpt containing the match.
    pub excerpt: String,
}

/// Named reason search results or coverage are incomplete.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SearchOmission {
    MatchBudgetExhausted { limit: usize },
    PerFileMatchBudgetExhausted { path: String, limit: usize },
    OutputBudgetExhausted { limit: usize },
    FileBudgetExhausted { limit: usize },
    ScanBudgetExhausted { limit: u64 },
    BinaryFile { path: String },
}

/// Result of `workspace.search@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSearchOutput {
    pub matches: Vec<SearchMatch>,
    pub omissions: Vec<SearchOmission>,
    pub scanned_files: u64,
    pub scanned_bytes: u64,
}

/// The production `workspace.search@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceSearch;

impl Tool for WorkspaceSearch {
    type Input = WorkspaceSearchInput;
    type Output = WorkspaceSearchOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("workspace.search", "1.0.0").expect("a built-in tool identity"),
            "Search the workspace",
            "Searches non-ignored regular files in process with bounded regexes, matches, excerpts, traversal, and scanned bytes.",
            RiskLevel::Observe,
        )
    }

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        match input.path.as_deref() {
            Some(path) => {
                Ok(RequestEffects::default().with_path(boundary.contain(path)?, PathAccess::Read))
            }
            None => Ok(RequestEffects::default()),
        }
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        context.check_still_permitted()?;
        let pattern = compile_pattern(&input)?;
        let requested = input.path.as_deref().unwrap_or(".");
        let root = context.resolve(Path::new(requested))?;
        let (files, file_budget_hit) = collect_files(root.as_path(), context)?;
        let max_matches = input.max_matches.unwrap_or(DEFAULT_SEARCH_MAX_MATCHES);
        let max_per_file = input.max_per_file.unwrap_or(DEFAULT_SEARCH_MAX_PER_FILE);
        let max_total_bytes = input.max_total_bytes.unwrap_or(DEFAULT_SEARCH_TOTAL_BYTES);
        let max_excerpt = input.max_excerpt_bytes.unwrap_or(240);
        let mut output = WorkspaceSearchOutput {
            matches: Vec::new(),
            omissions: Vec::new(),
            scanned_files: 0,
            scanned_bytes: 0,
        };
        if file_budget_hit {
            output.omissions.push(SearchOmission::FileBudgetExhausted {
                limit: MAX_SEARCH_FILES,
            });
        }
        let mut output_bytes = 0_usize;
        'files: for path in &files {
            context.check_still_permitted()?;
            let relative = path.strip_prefix(context.workspace_root()).unwrap_or(path);
            let remaining = MAX_SEARCH_SCANNED_BYTES.saturating_sub(output.scanned_bytes);
            if remaining == 0 {
                output.omissions.push(SearchOmission::ScanBudgetExhausted {
                    limit: MAX_SEARCH_SCANNED_BYTES,
                });
                break;
            }
            let metadata = fs::metadata(path).map_err(ToolError::execution_failed)?;
            let mut bytes = Vec::new();
            File::open(path)
                .map_err(ToolError::execution_failed)?
                .take(remaining.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(ToolError::execution_failed)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > remaining {
                bytes.truncate(usize::try_from(remaining).unwrap_or(usize::MAX));
                output.omissions.push(SearchOmission::ScanBudgetExhausted {
                    limit: MAX_SEARCH_SCANNED_BYTES,
                });
            }
            output.scanned_files = output.scanned_files.saturating_add(1);
            output.scanned_bytes = output
                .scanned_bytes
                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            let (display_path, path_is_lossy, path_base64) = project_path(relative);
            let Ok(text) = std::str::from_utf8(&bytes) else {
                output
                    .omissions
                    .push(SearchOmission::BinaryFile { path: display_path });
                continue;
            };
            let mut per_file = 0_usize;
            for (line_index, line) in text.split_inclusive('\n').enumerate() {
                let line = line.strip_suffix('\n').unwrap_or(line);
                let line = line.strip_suffix('\r').unwrap_or(line);
                for found in pattern.find_iter(line) {
                    if output.matches.len() == max_matches {
                        output
                            .omissions
                            .push(SearchOmission::MatchBudgetExhausted { limit: max_matches });
                        break 'files;
                    }
                    if per_file == max_per_file {
                        output
                            .omissions
                            .push(SearchOmission::PerFileMatchBudgetExhausted {
                                path: display_path.clone(),
                                limit: max_per_file,
                            });
                        break;
                    }
                    let excerpt = excerpt(line, found.start(), found.end(), max_excerpt);
                    let candidate = SearchMatch {
                        path: display_path.clone(),
                        path_is_lossy,
                        path_base64: path_base64.clone(),
                        line_number: u64::try_from(line_index + 1).unwrap_or(u64::MAX),
                        byte_column: u64::try_from(found.start()).unwrap_or(u64::MAX),
                        excerpt,
                    };
                    let candidate_bytes = serde_json::to_vec(&candidate)
                        .map_err(ToolError::execution_failed)?
                        .len();
                    if output_bytes.saturating_add(candidate_bytes) > max_total_bytes {
                        output
                            .omissions
                            .push(SearchOmission::OutputBudgetExhausted {
                                limit: max_total_bytes,
                            });
                        break 'files;
                    }
                    output_bytes += candidate_bytes;
                    output.matches.push(candidate);
                    per_file += 1;
                }
            }
            if metadata.len() > u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
                break;
            }
        }
        if serde_json::to_vec(&output)
            .map_err(ToolError::execution_failed)?
            .len()
            > max_total_bytes
        {
            output.matches.clear();
            output.omissions.clear();
            output
                .omissions
                .push(SearchOmission::OutputBudgetExhausted {
                    limit: max_total_bytes,
                });
        }
        Ok(output)
    }
}

fn compile_pattern(input: &WorkspaceSearchInput) -> Result<Regex, ToolError> {
    let source = if input.regex.unwrap_or(false) {
        input.query.clone()
    } else {
        regex::escape(&input.query)
    };
    RegexBuilder::new(&source)
        .case_insensitive(!input.case_sensitive.unwrap_or(true))
        .size_limit(1024 * 1024)
        .dfa_size_limit(1024 * 1024)
        .build()
        .map_err(|error| ToolError::execution_failed(format!("invalid search regex: {error}")))
}

fn collect_files(
    root: &Path,
    context: &ExecutionContext,
) -> Result<(Vec<PathBuf>, bool), ToolError> {
    let metadata = fs::symlink_metadata(root).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ToolError::NotFound {
            path: root.to_path_buf(),
        },
        _ => ToolError::execution_failed(error),
    })?;
    if metadata.is_file() {
        let relative = root
            .strip_prefix(context.workspace_root())
            .unwrap_or(root)
            .to_path_buf();
        let service =
            harkness_git::GitService::new(context.workspace_root(), context.workspace_root());
        let ignored = service.ignored_paths(&[relative]).map_err(map_git_error)?[0];
        return Ok((
            (!ignored).then(|| root.to_path_buf()).into_iter().collect(),
            false,
        ));
    }
    if !metadata.is_dir() {
        return Err(ToolError::execution_failed(format!(
            "{} is not a regular file or directory",
            root.display()
        )));
    }
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let service = harkness_git::GitService::new(context.workspace_root(), context.workspace_root());
    while let Some(directory) = directories.pop() {
        context.check_still_permitted()?;
        let entries =
            harkness_core::list_directory(&directory).map_err(ToolError::execution_failed)?;
        let relatives = entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .strip_prefix(context.workspace_root())
                    .unwrap_or(&entry.path)
                    .to_path_buf()
            })
            .collect::<Vec<_>>();
        let ignored = service.ignored_paths(&relatives).map_err(map_git_error)?;
        for ((entry, relative), ignored) in entries.into_iter().zip(relatives).zip(ignored) {
            if ignored || relative == Path::new(".git") {
                continue;
            }
            let metadata =
                fs::symlink_metadata(&entry.path).map_err(ToolError::execution_failed)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if entry.is_dir {
                directories.push(entry.path);
            } else if metadata.is_file() {
                if files.len() == MAX_SEARCH_FILES {
                    return Ok((files, true));
                }
                files.push(entry.path);
            }
        }
    }
    files.sort();
    Ok((files, false))
}

fn excerpt(line: &str, start: usize, end: usize, maximum: usize) -> String {
    if line.len() <= maximum {
        return line.to_owned();
    }
    let match_len = end.saturating_sub(start);
    let desired = maximum.max(match_len);
    let mut from = start.saturating_sub(desired.saturating_sub(match_len) / 2);
    let mut to = from.saturating_add(desired).min(line.len());
    if to - from < desired {
        from = to.saturating_sub(desired);
    }
    while from < line.len() && !line.is_char_boundary(from) {
        from += 1;
    }
    while to > from && !line.is_char_boundary(to) {
        to -= 1;
    }
    line[from..to].to_owned()
}
