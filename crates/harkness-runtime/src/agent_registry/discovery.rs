//! Enumerating agent executables without running any of them.
//!
//! Discovery is a convenience and never an authority. It answers one question —
//! "is a program with one of these names on the search path" — by looking at
//! directory entries, and it answers it with a path. Nothing here spawns a
//! process, opens a file, reads a byte of one, or hashes anything: a probe that
//! executed candidates "to check them" would turn `ls`-equivalent enumeration
//! into arbitrary code execution, which is the exact failure this design exists
//! to prevent. The tests hold that boundary directly, by pointing discovery at
//! shim executables that record every invocation and asserting there are none.
//!
//! What a candidate becomes is a *suggestion*. Registering one is a user action,
//! trusting it is a second, and enabling it is a third; none of them happens
//! here.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;

/// Executable names discovery looks for when a caller names none.
///
/// A suggestion list rather than a compatibility claim: an entry here means
/// "people commonly install an ACP agent under this name", and finding one
/// tells a user where to look rather than telling Harkness anything. Adding a
/// name costs nothing and grants nothing.
pub const DEFAULT_AGENT_CANDIDATES: &[&str] =
    &["claude-code-acp", "codex-acp", "gemini", "opencode"];

/// Most candidate names one probe may look for.
pub const MAX_DISCOVERY_CANDIDATES: usize = 64;
/// Most search-path directories one probe may look in.
pub const MAX_DISCOVERY_DIRECTORIES: usize = 64;
/// How long one probe may run before it reports what it has.
pub const DEFAULT_DISCOVERY_BUDGET: Duration = Duration::from_secs(5);
/// How often a probe checks its cancellation token and its deadline.
///
/// Every directory and every candidate is one `metadata` call, so the check is
/// per entry rather than on a timer — an order of magnitude inside the
/// workspace's 250 ms visibility target on any filesystem that answers at all.
const POLL_EVERY_ENTRIES: usize = 8;

/// One executable found on the search path, and nothing else about it.
///
/// There is deliberately no digest, no version, and no capability set. Every one
/// of those would require running or reading the program, and a candidate is by
/// definition something nobody has decided to trust yet.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiscoveredCandidate {
    /// The candidate name that matched.
    pub name: String,
    /// Where it was found, as an absolute path built from a search-path entry.
    pub resolved_path: PathBuf,
}

/// Why a probe stopped before it had looked everywhere.
///
/// A named answer rather than a short list: a truncated probe that said nothing
/// would read as "there is nothing else installed", which is the one conclusion
/// it cannot support.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum DiscoveryTruncation {
    /// The search path held more directories than the probe may look in.
    DirectoryBudget,
    /// The time budget expired.
    Deadline,
    /// The caller cancelled.
    Cancelled,
}

impl DiscoveryTruncation {
    /// The stable spelling a surface reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectoryBudget => "directory_budget",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for DiscoveryTruncation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What one probe found, and how far it got.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryReport {
    candidates: Vec<DiscoveredCandidate>,
    directories_searched: usize,
    truncation: Option<DiscoveryTruncation>,
}

impl DiscoveryReport {
    /// Every candidate found, in the order the search path implies.
    pub fn candidates(&self) -> impl ExactSizeIterator<Item = &DiscoveredCandidate> {
        self.candidates.iter()
    }

    /// How many search-path directories were looked in.
    #[must_use]
    pub const fn directories_searched(&self) -> usize {
        self.directories_searched
    }

    /// Why the probe stopped early, when it did.
    #[must_use]
    pub const fn truncation(&self) -> Option<DiscoveryTruncation> {
        self.truncation
    }

    /// Whether the probe looked everywhere it was asked to.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.truncation.is_none()
    }
}

/// How far, and for what, one probe looks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Discovery {
    candidates: Vec<String>,
    search_path: Option<OsString>,
    budget: Duration,
    max_directories: usize,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            candidates: DEFAULT_AGENT_CANDIDATES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            search_path: None,
            budget: DEFAULT_DISCOVERY_BUDGET,
            max_directories: MAX_DISCOVERY_DIRECTORIES,
        }
    }
}

impl Discovery {
    /// Replaces the candidate names, keeping at most
    /// [`MAX_DISCOVERY_CANDIDATES`] of them.
    ///
    /// A name carrying a path separator is dropped rather than refused: the
    /// value is joined onto a search-path directory, and a caller that wrote
    /// `../../bin/agent` would otherwise have discovery report a path outside
    /// the directory it claims to have searched.
    #[must_use]
    pub fn looking_for<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.candidates = names
            .into_iter()
            .map(Into::into)
            .filter(|name| !name.is_empty() && Path::new(name).components().count() == 1)
            .take(MAX_DISCOVERY_CANDIDATES)
            .collect();
        self
    }

    /// Searches this value instead of the process's own `PATH`.
    #[must_use]
    pub fn on_path(mut self, path: impl Into<OsString>) -> Self {
        self.search_path = Some(path.into());
        self
    }

    /// Replaces the time budget.
    #[must_use]
    pub const fn within(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Replaces the number of search-path directories the probe may look in.
    #[must_use]
    pub const fn across_at_most(mut self, directories: usize) -> Self {
        self.max_directories = directories;
        self
    }

    /// Looks for every candidate, and runs nothing.
    ///
    /// The search path is walked in order and the first directory holding a name
    /// wins it, which is the resolution a shell would perform — so the reported
    /// path is the program that *would* run, not merely one that shares a name.
    /// An unreadable or missing directory is skipped rather than failing the
    /// probe: a stale `PATH` entry is ordinary and is not a reason to tell a
    /// user that discovery is broken.
    #[must_use]
    pub fn run(&self, cancel: &Cancellation) -> DiscoveryReport {
        let started = Instant::now();
        let mut report = DiscoveryReport::default();
        if self.candidates.is_empty() {
            return report;
        }

        let search_path = self
            .search_path
            .clone()
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let directories = std::env::split_paths(&search_path).collect::<Vec<_>>();
        let mut remaining = self
            .candidates
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let mut checked = 0usize;

        for directory in directories {
            if remaining.is_empty() {
                break;
            }
            if report.directories_searched == self.max_directories {
                report.truncation = Some(DiscoveryTruncation::DirectoryBudget);
                break;
            }
            // An empty entry means "the current directory" to a shell, and the
            // current directory of a Harkness process invoked from a Git hook is
            // not a place a suggestion may come from.
            if directory.as_os_str().is_empty() {
                continue;
            }
            report.directories_searched += 1;

            let mut found_here = Vec::new();
            for name in &remaining {
                checked += 1;
                if checked.is_multiple_of(POLL_EVERY_ENTRIES) {
                    if cancel.is_cancelled() {
                        report.truncation = Some(DiscoveryTruncation::Cancelled);
                        return report;
                    }
                    if started.elapsed() >= self.budget {
                        report.truncation = Some(DiscoveryTruncation::Deadline);
                        return report;
                    }
                }
                let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
                if is_runnable_file(&path) {
                    report.candidates.push(DiscoveredCandidate {
                        name: (*name).to_owned(),
                        resolved_path: path,
                    });
                    found_here.push(*name);
                }
            }
            remaining.retain(|name| !found_here.contains(name));
        }

        report
    }
}

/// Whether a path names something a shell would be willing to execute.
///
/// `metadata` follows symlinks, which is right here: the reported path is the
/// one that would run, and a link to a program is a program. Nothing is opened
/// and nothing is read — the executable bit is a property of the directory
/// entry, and a candidate is by definition a program nobody has decided to
/// trust.
#[cfg(unix)]
fn is_runnable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether a path names a file.
///
/// Windows has no executable bit; whether a file can be started is decided by
/// its extension, which `EXE_SUFFIX` has already supplied.
#[cfg(not(unix))]
fn is_runnable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}
