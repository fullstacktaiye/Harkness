//! Bounded in-process workspace search with Git ignore awareness.

use std::io::Read;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::tool::{
    ExecutionContext, RequestEffects, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata,
};
use crate::trust::{PathAccess, PathBoundary};
use crate::trust::ContainedPath;

use super::git_status::{map_git_error, project_path};
use super::safe_read::{ensure_no_symlink_components, list_directory, open_regular};

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
/// Hard combined number of files and directories retained by traversal.
pub const MAX_SEARCH_ENTRIES: usize = 10_000;
/// Hard bytes inspected by one search.
pub const MAX_SEARCH_SCANNED_BYTES: u64 = 64 * 1024 * 1024;

/// Input to `workspace.search@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq)]
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSearchInputWire {
    query: String,
    path: Option<String>,
    regex: Option<bool>,
    case_sensitive: Option<bool>,
    max_matches: Option<usize>,
    max_per_file: Option<usize>,
    max_total_bytes: Option<usize>,
    max_excerpt_bytes: Option<usize>,
}

impl<'de> Deserialize<'de> for WorkspaceSearchInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceSearchInputWire::deserialize(deserializer)?;
        compile_regex(
            &wire.query,
            wire.regex.unwrap_or(false),
            wire.case_sensitive.unwrap_or(true),
        )
        .map_err(D::Error::custom)?;
        Ok(Self {
            query: wire.query,
            path: wire.path,
            regex: wire.regex,
            case_sensitive: wire.case_sensitive,
            max_matches: wire.max_matches,
            max_per_file: wire.max_per_file,
            max_total_bytes: wire.max_total_bytes,
            max_excerpt_bytes: wire.max_excerpt_bytes,
        })
    }
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
    MatchBudgetExhausted {
        limit: usize,
    },
    PerFileMatchBudgetExhausted {
        path: String,
        path_is_lossy: bool,
        path_base64: Option<String>,
        limit: usize,
    },
    OutputBudgetExhausted {
        limit: usize,
    },
    FileBudgetExhausted {
        limit: usize,
    },
    ScanBudgetExhausted {
        limit: u64,
    },
    BinaryFile {
        path: String,
        path_is_lossy: bool,
        path_base64: Option<String>,
    },
    NonRegularFile {
        path: String,
        path_is_lossy: bool,
        path_base64: Option<String>,
    },
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
        reject_git_administration_path(Path::new(requested))?;
        reject_symlink_root(Path::new(requested), context)?;
        let root = context.resolve(Path::new(requested))?;
        let (files, file_budget_hit, non_regular) = collect_files(&root, context)?;
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
        for path in non_regular {
            let (path, path_is_lossy, path_base64) = project_path(&path);
            output.omissions.push(SearchOmission::NonRegularFile {
                path: context.redact_text(&path),
                path_is_lossy,
                path_base64,
            });
        }
        let mut output_bytes = 0_usize;
        'files: for path in &files {
            context.check_still_permitted()?;
            let path = path.revalidate().map_err(ToolError::from)?;
            let relative = path
                .as_path()
                .strip_prefix(context.workspace_root())
                .unwrap_or(path.as_path());
            let remaining = MAX_SEARCH_SCANNED_BYTES.saturating_sub(output.scanned_bytes);
            if remaining == 0 {
                output.omissions.push(SearchOmission::ScanBudgetExhausted {
                    limit: MAX_SEARCH_SCANNED_BYTES,
                });
                break;
            }
            let (mut file, metadata) = open_regular(&path)?;
            let (display_path, path_is_lossy, path_base64) = project_path(relative);
            let display_path = context.redact_text(&display_path);
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 8192];
            while u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= remaining {
                context.check_still_permitted()?;
                let read = file
                    .read(&mut buffer)
                    .map_err(ToolError::execution_failed)?;
                if read == 0 {
                    break;
                }
                let keep = usize::try_from(remaining.saturating_add(1))
                    .unwrap_or(usize::MAX)
                    .saturating_sub(bytes.len())
                    .min(read);
                bytes.extend_from_slice(&buffer[..keep]);
                if keep < read {
                    break;
                }
            }
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
            let Ok(text) = std::str::from_utf8(&bytes) else {
                output.omissions.push(SearchOmission::BinaryFile {
                    path: display_path,
                    path_is_lossy,
                    path_base64,
                });
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
                                path_is_lossy,
                                path_base64: path_base64.clone(),
                                limit: max_per_file,
                            });
                        break;
                    }
                    let excerpt = context.redact_text(&excerpt(
                        line,
                        found.start(),
                        found.end(),
                        max_excerpt,
                    ));
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
    compile_regex(
        &input.query,
        input.regex.unwrap_or(false),
        input.case_sensitive.unwrap_or(true),
    )
    .map_err(|error| ToolError::execution_failed(format!("invalid search regex: {error}")))
}

fn compile_regex(query: &str, is_regex: bool, case_sensitive: bool) -> Result<Regex, regex::Error> {
    let source = if is_regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .size_limit(1024 * 1024)
        .dfa_size_limit(1024 * 1024)
        .build()
}

fn collect_files(
    root: &ContainedPath,
    context: &ExecutionContext,
) -> Result<(Vec<ContainedPath>, bool, Vec<std::path::PathBuf>), ToolError> {
    let root = root.revalidate().map_err(ToolError::from)?;
    if open_regular(&root).is_ok() {
        let relative = root
            .as_path()
            .strip_prefix(context.workspace_root())
            .unwrap_or(root.as_path())
            .to_path_buf();
        let service =
            harkness_git::GitService::new(context.workspace_root(), context.workspace_root());
        let ignored = service.ignored_paths(&[relative]).map_err(map_git_error)?[0];
        return Ok((
            (!ignored).then_some(root).into_iter().collect(),
            false,
            Vec::new(),
        ));
    }
    // Probe the directory through the same held-descriptor route the walk uses.
    list_directory(&root, 0, context)?;
    let mut directories = vec![root];
    let mut files = Vec::new();
    let mut non_regular = Vec::new();
    let mut visited = 0_usize;
    let service = harkness_git::GitService::new(context.workspace_root(), context.workspace_root());
    while let Some(directory) = directories.pop() {
        context.check_still_permitted()?;
        let directory = directory.revalidate().map_err(ToolError::from)?;
        let remaining = MAX_SEARCH_ENTRIES.saturating_sub(visited);
        if remaining == 0 {
            return Ok((files, true, non_regular));
        }
        let listing = list_directory(&directory, remaining, context)?;
        let listing_omitted = listing.truncated;
        let entries = listing.entries;
        visited = visited.saturating_add(entries.len());
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
            if entry.is_symlink {
                continue;
            }
            let contained = context.resolve(&entry.path)?;
            if entry.is_dir {
                directories.push(contained);
            } else if entry.is_file {
                if files.len() == MAX_SEARCH_FILES {
                    return Ok((files, true, non_regular));
                }
                files.push(contained);
            } else {
                non_regular.push(relative);
            }
        }
        if listing_omitted {
            return Ok((files, true, non_regular));
        }
    }
    files.sort_by(|left, right| left.as_path().cmp(right.as_path()));
    Ok((files, false, non_regular))
}

fn excerpt(line: &str, start: usize, end: usize, maximum: usize) -> String {
    if line.len() <= maximum {
        return line.to_owned();
    }
    let match_len = end.saturating_sub(start).min(maximum);
    let mut from = start.saturating_sub(maximum.saturating_sub(match_len) / 2);
    let mut to = from.saturating_add(maximum).min(line.len());
    if to - from < maximum {
        from = to.saturating_sub(maximum);
    }
    while from < line.len() && !line.is_char_boundary(from) {
        from += 1;
    }
    while to > from && !line.is_char_boundary(to) {
        to -= 1;
    }
    line[from..to].to_owned()
}

fn reject_git_administration_path(path: &Path) -> Result<(), ToolError> {
    if path
        .components()
        .any(|component| component.as_os_str() == ".git")
    {
        return Err(ToolError::ForbiddenPath {
            path: path.to_path_buf(),
            reason: "Git administration directories are not searchable".to_owned(),
        });
    }
    Ok(())
}

fn reject_symlink_root(path: &Path, context: &ExecutionContext) -> Result<(), ToolError> {
    let lexical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        context.workspace_root().join(path)
    };
    ensure_no_symlink_components(&lexical)
}
