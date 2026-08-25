//! The scan itself: index rows in, bounded matches out.
//!
//! # The universe is the index, and it is never the filesystem
//!
//! Every file this module opens came out of a `files` row, and every row came
//! out of an inventory walk whose four exclusion layers had already decided
//! what Harkness may read. There is no walk here and there must never be one:
//! a fallback that listed a directory when the index was cold would search
//! paths no layer ever examined, and a `.env` would be one `read_dir` away from
//! a model's context. A worktree the cache has never seen is therefore
//! [`SearchError::IndexUnavailable`] rather than an empty answer.
//!
//! # Ordering is a total order over positions
//!
//! Matches sort by canonical path bytes ascending, then by absolute byte offset
//! ascending, and nothing else ever decides. Both come out of the scan already
//! sorted — the index yields rows in path order and a file is scanned front to
//! back — so there is no sort to be stable or unstable about. A content match
//! is reported once per matching line and positioned at the first occurrence on
//! that line, which is what makes the pair unique: two matches sharing a
//! position would be two matches a cursor could not sit between.
//!
//! # The merge, and why prefixes cannot simply be concatenated
//!
//! A query narrowed to several subtrees reads one ordered stream per subtree
//! and merges them, rather than reading them one after another. Sorted prefixes
//! do *not* imply sorted paths: `sr-x` sorts after `sr`, but `sr-x/a` sorts
//! before `sr/a`, because `-` is below `/`. Concatenating would produce an
//! order no cursor could resume from.

use std::io;
use std::path::Path;

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use harkness_git::Cancellation;

use crate::chunk::ContentEncoding;
use crate::classify::OVERSIZED_FILE_THRESHOLD;
use crate::digest::Sha256Hex;
use crate::error::ContextEngineError;
use crate::ids::{ContextQueryId, SnapshotId};
use crate::index::{IndexCache, IndexedFile, WorktreeKey};
use crate::path::RepoPath;
use crate::provenance::{
    ByteRange, Provenance, RetrievalSource, SelectionReason, SelectionReasonKind,
};

use super::cursor::SearchCursor;
use super::error::SearchError;
use super::query::{
    MAX_PATTERN_BYTES, MAX_REGEX_SIZE_BYTES, SearchLimits, SearchPattern, SearchQuery,
};
use super::result::{
    BoundedText, MAX_SEARCH_OMISSIONS, SearchMatch, SearchOmission, SearchResponse, SearchStats,
};

/// How many index rows one page of the row merge holds.
///
/// One page per narrowed subtree is live at a time, so the value is paid in
/// memory `MAX_PATH_PREFIXES` times over. Nothing depends on it: the merge is
/// correct at any page size because every stream is ordered by the same bytes.
const ROW_PAGE: usize = 1_024;

/// The byte a NUL-detecting scan stops at.
const NUL: u8 = 0x00;

/// Little-endian UTF-16 byte-order mark.
const UTF16_LE_BOM: &[u8] = &[0xff, 0xfe];

/// Big-endian UTF-16 byte-order mark.
const UTF16_BE_BOM: &[u8] = &[0xfe, 0xff];

/// One search, ready to run.
///
/// The pattern is compiled and every bound resolved before the first row is
/// read, so a query that cannot run costs no I/O at all.
pub(crate) struct Plan {
    kind: &'static str,
    matcher: RegexMatcher,
    /// Whether the matcher is run against paths rather than against content.
    over_paths: bool,
    source: RetrievalSource,
    reason: SelectionReason,
    limits: SearchLimits,
    identity: Sha256Hex,
    cursor: Option<SearchCursor>,
    /// The generation the cursor was admitted against and the next one is
    /// minted from, read once so the two cannot disagree.
    generation: u64,
}

impl Plan {
    /// Validates `query` and compiles what it needs.
    fn compile(query: &SearchQuery, generation: u64) -> Result<Self, SearchError> {
        let pattern = query.pattern();
        let kind = pattern.kind();
        let invalid = |reason: String| SearchError::InvalidPattern {
            pattern_kind: kind,
            reason,
        };
        let text = pattern.text();
        if text.is_empty() {
            return Err(invalid("the pattern is empty".to_owned()));
        }
        if text.len() > MAX_PATTERN_BYTES {
            return Err(invalid(format!(
                "the pattern is {} bytes, past the {MAX_PATTERN_BYTES}-byte limit",
                text.len()
            )));
        }
        if matches!(pattern, SearchPattern::Regex(_)) && !query.regex_permitted() {
            return Err(SearchError::RegexNotPermitted);
        }

        let identity = query.identity();
        let cursor = match query.cursor() {
            Some(cursor) => {
                cursor.admits(generation, &identity)?;
                Some(cursor.clone())
            }
            None => None,
        };

        // One engine decides what "matches" means, for all three shapes. A
        // hand-rolled substring search over paths would be a second answer to
        // the same question, and the two would be free to disagree about a
        // pattern nobody thought to test with.
        let literal = !matches!(pattern, SearchPattern::Regex(_));
        let matcher = build_matcher(text, literal).map_err(|error| {
            // The engine's own message names the pattern it refused, which is
            // a caller's text: it is carried because a person fixing a regular
            // expression needs to know what was wrong with theirs, and a
            // refusal does not reach the diagnostic log that spans are careful
            // to keep patterns out of.
            invalid(error.to_string())
        })?;
        let (over_paths, source, detail) = match pattern {
            SearchPattern::Filename(_) => (
                true,
                RetrievalSource::FilenameSearch,
                "the repository-relative path contains the requested text",
            ),
            SearchPattern::Exact(_) => (
                false,
                RetrievalSource::LexicalSearch,
                "the line contains the requested literal text",
            ),
            SearchPattern::Regex(_) => (
                false,
                RetrievalSource::LexicalSearch,
                "the line matches the requested regular expression",
            ),
        };

        Ok(Self {
            kind,
            matcher,
            over_paths,
            source,
            reason: SelectionReason::new(SelectionReasonKind::QueryMatch, detail),
            limits: *query.limits(),
            identity,
            cursor,
            generation,
        })
    }
}

/// Compiles one pattern into the line-oriented matcher the searcher runs.
///
/// Three settings are guarantees rather than tuning. `fixed_strings` is what
/// makes an exact search exact — every metacharacter is escaped, so nothing a
/// caller writes is interpreted, which is why the exact shape needs no
/// capability. `line_terminator` refuses at build time any pattern that could
/// match across a line, so a line-oriented scan cannot be handed a pattern that
/// is not line-oriented. `ban_byte` refuses a pattern containing the NUL that
/// binary detection stops at, so a pattern can never be written that only
/// matches content the scan will not reach.
fn build_matcher(pattern: &str, literal: bool) -> Result<RegexMatcher, grep_regex::Error> {
    RegexMatcherBuilder::new()
        .fixed_strings(literal)
        .multi_line(false)
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(NUL))
        .size_limit(MAX_REGEX_SIZE_BYTES)
        .build(pattern)
}

/// Everything a scan reads from, fixed before the first row.
pub(crate) struct Scan<'engine> {
    /// The cache the universe is read out of.
    pub(crate) cache: &'engine IndexCache,
    /// Which checkout's rows to read.
    pub(crate) worktree: &'engine WorktreeKey,
    /// The canonical worktree root every path is resolved against.
    pub(crate) root: &'engine Path,
}

impl Scan<'_> {
    /// Everything a query can be refused for, decided with no capture taken.
    ///
    /// Separate from [`run`](Self::run) because the capture that stamps the
    /// answer reads the *whole workspace* — several times the cost of the scan
    /// on a repository of any size — and a query that cannot run must not cost
    /// one. It is also the amplification a caller could otherwise reach for:
    /// repeating a regular-expression query it holds no capability for would
    /// drive an unbounded number of full workspace reads for an answer that was
    /// always going to be `regex_not_permitted`.
    pub(crate) fn prepare(&self, query: &SearchQuery) -> Result<Plan, ContextEngineError> {
        // Asked before anything is read, and answered from the watermark rather
        // than from a row count: a worktree with rows is indexed, and one
        // without them is a question this module must not answer with "no
        // match".
        if self.cache.worktree_generation(self.worktree)? == 0 {
            return Err(SearchError::IndexUnavailable {
                worktree: self.worktree.as_str().to_owned(),
                reason: "no batch has ever published this checkout; build the index first",
            }
            .into());
        }
        Ok(Plan::compile(query, self.cache.generation())?)
    }

    /// Runs a prepared query and returns one bounded page.
    pub(crate) fn run(
        &self,
        query: &SearchQuery,
        plan: &Plan,
        snapshot: SnapshotId,
        cancellation: &Cancellation,
    ) -> Result<SearchResponse, ContextEngineError> {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        let generation = plan.generation;
        let mut rows = RowStream::open(self.cache, self.worktree, query, plan.cursor.as_ref())?;
        let mut progress = Progress {
            collector: Collector::new(plan.limits),
            stats: SearchStats::default(),
            // One searcher for the whole scan rather than one per file: it owns
            // a line buffer, and a query over ten thousand files would
            // otherwise build and drop ten thousand of them.
            searcher: line_searcher(),
        };

        while let Some(row) = rows.next(cancellation)? {
            if cancellation.is_cancelled() {
                return Err(ContextEngineError::Cancelled);
            }
            progress.stats.paths_examined += 1;
            if !row.eligible() || !query.filters().admits(row.class) {
                continue;
            }
            let carried = if plan.over_paths {
                self.offer_filename(&row, plan, snapshot, &mut progress)
            } else {
                self.scan_content(&row, plan, snapshot, &mut progress, cancellation)?
            };
            if !carried {
                break;
            }
        }

        let Progress {
            collector, stats, ..
        } = progress;
        let (matches, omissions, dropped, truncated) = collector.finish();
        // A cursor only when a bound actually fired. A page that happens to hold
        // exactly `max_results` matches because the repository holds exactly
        // that many is a complete answer, and handing it a continuation would
        // make every complete answer look truncated.
        let next_cursor = truncated.then(|| matches.last()).flatten().map(|last| {
            SearchCursor::new(
                generation,
                plan.identity.clone(),
                last.path.clone(),
                last.byte_offset,
            )
        });

        // The pattern *kind* and never the pattern. A diagnostic log is durable
        // and shared; what somebody searched their own repository for is not
        // something a later reader of that file is owed.
        tracing::debug!(
            pattern_kind = plan.kind,
            paths_examined = stats.paths_examined,
            files_scanned = stats.files_scanned,
            bytes_read = stats.bytes_read,
            matches = matches.len(),
            omissions = omissions.len(),
            dropped_omissions = dropped,
            truncated = next_cursor.is_some(),
            "context search completed"
        );

        Ok(SearchResponse {
            query_id: ContextQueryId::new(),
            snapshot_id: snapshot,
            index_generation: generation,
            matches,
            omissions,
            dropped_omissions: dropped,
            next_cursor,
            stats,
        })
    }

    /// Offers one filename match, if the path holds the requested substring.
    ///
    /// Returns whether the scan may continue.
    fn offer_filename(
        &self,
        row: &IndexedFile,
        plan: &Plan,
        snapshot: SnapshotId,
        progress: &mut Progress,
    ) -> bool {
        if !plan.matcher.is_match(row.path.as_bytes()).unwrap_or(false) {
            return true;
        }
        // The cursor's own row is fetched by name, because a *content* page may
        // have stopped in the middle of a file. A filename match holds one
        // position per file, so the seeded row is always one the previous page
        // already returned — and offering it again is how the same match ends
        // every page and starts the next.
        if plan
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.precedes(&row.path, 0))
        {
            return true;
        }
        let line = BoundedText::clamped(row.path.as_bytes(), plan.limits.max_line_bytes());
        let mut provenance = Provenance::new(
            plan.source,
            snapshot,
            &line.shown_bytes(),
            plan.reason.clone(),
        )
        .at_path(row.path.clone());
        if line.is_truncated() {
            provenance = provenance.truncated();
        }
        progress.collector.offer(SearchMatch {
            path: row.path.clone(),
            byte_offset: 0,
            line_number: None,
            line,
            before: Vec::new(),
            after: Vec::new(),
            // Nothing was read, so there is no file version to name. The
            // provenance digest still covers exactly what was shown — the path
            // — which is the thing it is documented to mean.
            content_sha256: None,
            provenance,
        })
    }

    /// Reads one file and offers every matching line in it.
    ///
    /// Returns whether the scan may continue.
    fn scan_content(
        &self,
        row: &IndexedFile,
        plan: &Plan,
        snapshot: SnapshotId,
        progress: &mut Progress,
        cancellation: &Cancellation,
    ) -> Result<bool, ContextEngineError> {
        let Progress {
            collector,
            stats,
            searcher,
        } = progress;
        let absolute = self.root.join(row.path.to_path_buf());
        let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
            collector.omit(SearchOmission::FileUnreadable {
                path: row.path.clone(),
            });
            return Ok(true);
        };
        // A path that is no longer a regular file is not a file that could not
        // be read: it is a different entry, and the next reconcile is what
        // records it as one. Never followed here, for the reason the walk never
        // follows one — the content is somewhere the inventory did not examine.
        if !metadata.is_file() {
            collector.omit(SearchOmission::FileChangedSinceIndex {
                path: row.path.clone(),
            });
            return Ok(true);
        }
        // A file that has grown past the eligibility threshold since it was
        // indexed is one the inventory would no longer offer, so not searching
        // it is the correct answer rather than a shortcut — and saying the index
        // is behind is what keeps that from being silent.
        if metadata.len() > OVERSIZED_FILE_THRESHOLD {
            collector.omit(SearchOmission::FileChangedSinceIndex {
                path: row.path.clone(),
            });
            return Ok(true);
        }
        let Some(bytes) = read_bounded(&absolute, metadata.len()) else {
            collector.omit(SearchOmission::FileUnreadable {
                path: row.path.clone(),
            });
            return Ok(true);
        };
        let mut moved = metadata.len() != row.byte_size
            || (row.mtime_ns.is_some()
                && crate::inventory::modified_nanos(&metadata) != row.mtime_ns);

        // A UTF-16 file is text the chunker transcodes and this scan does not.
        // Transcoding here would report offsets into a decoded stream no file on
        // disk holds, and byte offsets are what provenance and every later edit
        // are anchored to — so the honest answer is that the file was not
        // searched, said out loud rather than returned as "no match".
        if bytes.starts_with(UTF16_LE_BOM) || bytes.starts_with(UTF16_BE_BOM) {
            collector.omit(SearchOmission::EncodingNotSearchable {
                path: row.path.clone(),
                encoding: if bytes.starts_with(UTF16_LE_BOM) {
                    ContentEncoding::Utf16Le
                } else {
                    ContentEncoding::Utf16Be
                },
            });
            return Ok(true);
        }

        stats.files_scanned += 1;
        stats.bytes_read += bytes.len() as u64;

        let mut sink = MatchSink {
            plan,
            snapshot,
            collector,
            row,
            bytes: &bytes,
            digest: None,
            moved: &mut moved,
            reported_move: false,
            cancellation,
            full: false,
            cancelled: false,
        };
        let scanned = searcher.search_slice(&plan.matcher, &bytes, &mut sink);
        let stop = sink.full;
        let cancelled = sink.cancelled;
        let reported = sink.reported_move;
        if cancelled {
            return Err(ContextEngineError::Cancelled);
        }
        // The sink itself never fails, so this is the searcher refusing the
        // file — a configuration the matcher and the searcher disagree about.
        // Reported as unreadable rather than swallowed: a file that was not
        // examined must never be indistinguishable from one that held no match.
        if scanned.is_err() {
            collector.omit(SearchOmission::FileUnreadable {
                path: row.path.clone(),
            });
            return Ok(true);
        }
        if moved && !reported {
            collector.omit(SearchOmission::FileChangedSinceIndex {
                path: row.path.clone(),
            });
        }
        Ok(!stop)
    }
}

/// The searcher every content scan runs on.
///
/// Line-oriented and non-transcoding by construction: `bom_sniffing` is off so
/// a byte offset always names a byte of the file rather than of a decoded
/// stream, `multi_line` is off so a match is always exactly one line, and
/// context is *not* asked for here — it is cut from the file bytes afterwards,
/// so that which lines belong to which match is arithmetic rather than a
/// question about the order two sink callbacks arrived in.
fn line_searcher() -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .multi_line(false)
        .bom_sniffing(false)
        .binary_detection(BinaryDetection::quit(NUL))
        .build()
}

/// Reads a file, refusing to buffer more than an eligible one may hold.
///
/// The bound is enforced on the bytes *read* rather than on `expected`, because
/// a file grows between the `stat` and the open and a size taken from metadata
/// is a claim about a moment that has passed. `expected` decides only the
/// buffer, and getting that right is worth doing: `read_to_end` over a `Take`
/// has no size hint to work from, so it starts small and doubles, and a scan
/// that opens ten thousand files pays those extra reads ten thousand times.
fn read_bounded(path: &Path, expected: u64) -> Option<Vec<u8>> {
    use std::io::Read as _;

    let file = std::fs::File::open(path).ok()?;
    let capacity = usize::try_from(expected.min(OVERSIZED_FILE_THRESHOLD)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity.saturating_add(1));
    let mut reader = file.take(OVERSIZED_FILE_THRESHOLD.saturating_add(1));
    reader.read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > OVERSIZED_FILE_THRESHOLD {
        return None;
    }
    Some(bytes)
}

/// The mutable state one scan carries from file to file.
///
/// Carried as one value rather than as three parameters: they are threaded
/// together through every step of a scan, and a signature that named each of
/// them separately would grow by one every time a scan learned to count
/// something else.
struct Progress {
    collector: Collector,
    stats: SearchStats,
    searcher: Searcher,
}

/// Turns the searcher's per-line callbacks into offered matches.
struct MatchSink<'run> {
    plan: &'run Plan,
    snapshot: SnapshotId,
    collector: &'run mut Collector,
    row: &'run IndexedFile,
    bytes: &'run [u8],
    /// Computed at most once per file, and only when a match needs one.
    digest: Option<Sha256Hex>,
    moved: &'run mut bool,
    reported_move: bool,
    cancellation: &'run Cancellation,
    /// Set when a response budget fired, which stops the whole search.
    full: bool,
    cancelled: bool,
}

impl MatchSink<'_> {
    /// The digest of the bytes actually searched, computed on demand.
    ///
    /// Lazily, because a query over ten thousand files matches in a handful of
    /// them and hashing every file to answer about those few is the cost the
    /// index exists to avoid. Once computed it is compared against the row, so a
    /// file whose metadata matched but whose bytes did not is still reported —
    /// coarse modification times are a real filesystem, and the reconciler
    /// states the same residual for the same reason.
    fn digest(&mut self) -> Sha256Hex {
        if let Some(known) = self.digest.as_ref() {
            return known.clone();
        }
        let observed = Sha256Hex::of(self.bytes);
        if self.row.content_sha256.as_ref() != Some(&observed) {
            *self.moved = true;
        }
        self.digest.insert(observed).clone()
    }
}

impl Sink for MatchSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.cancellation.is_cancelled() {
            self.cancelled = true;
            return Ok(false);
        }
        let line_start = usize::try_from(mat.absolute_byte_offset()).unwrap_or(usize::MAX);
        let raw = mat.bytes();
        let content = strip_terminator(raw);
        let within = self
            .plan
            .matcher
            .find_at(raw, 0)
            .ok()
            .flatten()
            .map_or(0, |found| found.start());
        let byte_offset = (line_start + within) as u64;
        if self
            .plan
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.precedes(&self.row.path, byte_offset))
        {
            return Ok(true);
        }

        let limit = self.plan.limits.max_line_bytes();
        let line = BoundedText::clamped(content, limit);
        let context = usize::try_from(self.plan.limits.context_lines()).unwrap_or(0);
        let before = preceding_lines(self.bytes, line_start, context, limit);
        let after = following_lines(self.bytes, line_start + raw.len(), context, limit);
        let line_number = mat.line_number();
        let mut range = ByteRange::new(line_start as u64, (line_start + content.len()) as u64);
        if let Some(number) = line_number.and_then(|number| u32::try_from(number).ok()) {
            range = range.with_lines(number, number);
        }
        // The digest covers the matched line as emitted, which is what `range`
        // names. Context lines sit beside it as their own bounded texts and are
        // deliberately outside both: a digest spanning them would describe a
        // region the range does not, and the range is what an edit is applied
        // against.
        let mut provenance = Provenance::new(
            self.plan.source,
            self.snapshot,
            &line.shown_bytes(),
            self.plan.reason.clone(),
        )
        .at_path(self.row.path.clone())
        .in_range(range);
        if line.is_truncated() {
            provenance = provenance.truncated();
        }
        let digest = self.digest();
        if *self.moved && !self.reported_move {
            self.reported_move = true;
            self.collector.omit(SearchOmission::FileChangedSinceIndex {
                path: self.row.path.clone(),
            });
        }

        let carried = self.collector.offer(SearchMatch {
            path: self.row.path.clone(),
            byte_offset,
            line_number,
            line,
            before,
            after,
            content_sha256: Some(digest),
            provenance,
        });
        if !carried {
            self.full = true;
        }
        Ok(carried)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        self.collector.omit(SearchOmission::BinaryContentDetected {
            path: self.row.path.clone(),
            byte_offset: binary_byte_offset,
        });
        Ok(true)
    }
}

/// Drops a trailing `\n`, and the `\r` of a `\r\n` behind it.
fn strip_terminator(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// The `count` lines immediately before `start`, in file order.
fn preceding_lines(bytes: &[u8], start: usize, count: usize, limit: u64) -> Vec<BoundedText> {
    let mut lines = Vec::with_capacity(count);
    let mut end = start;
    for _ in 0..count {
        if end == 0 {
            break;
        }
        // `end` sits one past a line terminator, so the line before it ends at
        // `end - 1` and begins after the terminator before that.
        let terminated = end - 1;
        let begin = bytes[..terminated]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        lines.push(BoundedText::clamped(
            strip_terminator(&bytes[begin..end]),
            limit,
        ));
        end = begin;
    }
    lines.reverse();
    lines
}

/// The `count` lines immediately after `from`, in file order.
fn following_lines(bytes: &[u8], from: usize, count: usize, limit: u64) -> Vec<BoundedText> {
    let mut lines = Vec::with_capacity(count);
    let mut begin = from;
    for _ in 0..count {
        if begin >= bytes.len() {
            break;
        }
        let end = bytes[begin..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |position| begin + position + 1);
        lines.push(BoundedText::clamped(
            strip_terminator(&bytes[begin..end]),
            limit,
        ));
        begin = end;
    }
    lines
}

/// Accumulates matches until a published bound says to stop.
///
/// The bound is checked against the *offered* match rather than the stored one,
/// which is what makes a truncated page distinguishable from a complete one: a
/// match that does not fit is proof there was more, and a scan that simply ran
/// out never fires a bound at all. It is the same probe
/// [`IndexedPage`](crate::index::IndexedPage) uses on the read side, for the
/// same reason.
struct Collector {
    limits: SearchLimits,
    matches: Vec<SearchMatch>,
    omissions: Vec<SearchOmission>,
    dropped: usize,
    bytes: u64,
    /// The budget that stopped the scan, held apart from the rest.
    ///
    /// Not in `omissions` while the scan runs, because that list is capped and
    /// this one entry is what a caller reads as "there is more". A response
    /// whose omissions filled up with unreadable files would otherwise lose its
    /// truncation notice — and, since the cursor is emitted only when a bound
    /// fired, its continuation with it.
    stopped: Option<SearchOmission>,
}

impl Collector {
    fn new(limits: SearchLimits) -> Self {
        Self {
            limits,
            matches: Vec::new(),
            omissions: Vec::new(),
            dropped: 0,
            bytes: 0,
            stopped: None,
        }
    }

    /// Takes one match, answering whether the scan may continue.
    fn offer(&mut self, candidate: SearchMatch) -> bool {
        if self.matches.len() >= self.limits.max_results() {
            self.stopped = Some(SearchOmission::ResultBudgetExhausted {
                limit: self.limits.max_results(),
            });
            return false;
        }
        let cost = candidate.budget_cost();
        // A budget smaller than a single match returns that match anyway. The
        // alternative is an empty page carrying a cursor that points before the
        // very match that did not fit, which the next call would refuse in the
        // same way — a caller paging politely forever over nothing.
        if !self.matches.is_empty() && self.bytes.saturating_add(cost) > self.limits.max_bytes() {
            self.stopped = Some(SearchOmission::ByteBudgetExhausted {
                limit: self.limits.max_bytes(),
            });
            return false;
        }
        // Context lines are clamped by the same bound as the match line, so
        // they are asked about here too: a bound that fired with nothing in the
        // payload saying so is the one failure this list exists to prevent, and
        // a caller reading `matches` has no reason to inspect each context
        // line's own truncation flag.
        let clamped = candidate.line.is_truncated()
            || candidate
                .before
                .iter()
                .chain(candidate.after.iter())
                .any(BoundedText::is_truncated);
        if clamped {
            self.omit(SearchOmission::LineTooLong {
                path: candidate.path.clone(),
                byte_offset: candidate.byte_offset,
                limit: self.limits.max_line_bytes(),
            });
        }
        self.bytes = self.bytes.saturating_add(cost);
        self.matches.push(candidate);
        true
    }

    /// Records one omission, counting it once the list is full.
    fn omit(&mut self, omission: SearchOmission) {
        if self.omissions.len() >= MAX_SEARCH_OMISSIONS {
            self.dropped += 1;
            return;
        }
        self.omissions.push(omission);
    }

    /// The matches, the omissions with the budget last, whatever was dropped,
    /// and whether a bound fired.
    fn finish(mut self) -> (Vec<SearchMatch>, Vec<SearchOmission>, usize, bool) {
        let truncated = self.stopped.is_some();
        if let Some(stopped) = self.stopped.take() {
            self.omissions.push(stopped);
        }
        (self.matches, self.omissions, self.dropped, truncated)
    }
}

/// One prefix's ordered page of index rows.
struct PrefixStream {
    prefix: RepoPath,
    buffer: std::collections::VecDeque<IndexedFile>,
    after: Option<RepoPath>,
    exhausted: bool,
}

/// Index rows in one global path order, however many subtrees were named.
struct RowStream<'cache> {
    cache: &'cache IndexCache,
    worktree: &'cache WorktreeKey,
    /// The cursor's own file, which no `after` bound can include.
    seed: Option<IndexedFile>,
    streams: Vec<PrefixStream>,
}

impl<'cache> RowStream<'cache> {
    /// Opens one stream per narrowed subtree, seeded from `cursor`.
    ///
    /// A continuation reads rows strictly after the cursor's path and takes
    /// that path's own row separately, because a page may have stopped in the
    /// middle of a file and the rest of that file is the next page's first
    /// matches. Asking the cache for it by name is exact; asking for "the row
    /// before this one" is not expressible over arbitrary byte strings.
    fn open(
        cache: &'cache IndexCache,
        worktree: &'cache WorktreeKey,
        query: &SearchQuery,
        cursor: Option<&SearchCursor>,
    ) -> Result<Self, ContextEngineError> {
        let mut prefixes = query.filters().prefixes();
        if prefixes.is_empty() {
            prefixes.push(RepoPath::from_bytes(Vec::new()));
        }
        let seed = match cursor {
            Some(cursor)
                if prefixes
                    .iter()
                    .any(|prefix| prefix.is_empty() || prefix.contains(cursor.path())) =>
            {
                cache.file(worktree, cursor.path())?
            }
            _ => None,
        };
        let streams = prefixes
            .into_iter()
            .map(|prefix| PrefixStream {
                prefix,
                buffer: std::collections::VecDeque::new(),
                after: cursor.map(|cursor| cursor.path().clone()),
                exhausted: false,
            })
            .collect();
        Ok(Self {
            cache,
            worktree,
            seed,
            streams,
        })
    }

    /// The next row in global path order.
    fn next(
        &mut self,
        cancellation: &Cancellation,
    ) -> Result<Option<IndexedFile>, ContextEngineError> {
        if let Some(seed) = self.seed.take() {
            return Ok(Some(seed));
        }
        for stream in &mut self.streams {
            if cancellation.is_cancelled() {
                return Err(ContextEngineError::Cancelled);
            }
            if !stream.buffer.is_empty() || stream.exhausted {
                continue;
            }
            let page = self.cache.files_under(
                self.worktree,
                &stream.prefix,
                stream.after.as_ref(),
                ROW_PAGE,
            )?;
            // Only advanced by a page that held something. A page that came
            // back empty must leave the seek where it was, or the next call
            // would restart the stream from the beginning of the subtree.
            if let Some(last) = page.rows.last() {
                stream.after = Some(last.path.clone());
            } else {
                stream.exhausted = true;
            }
            stream.exhausted |= !page.more;
            stream.buffer.extend(page.rows);
        }
        let mut chosen: Option<usize> = None;
        for (index, stream) in self.streams.iter().enumerate() {
            let Some(head) = stream.buffer.front() else {
                continue;
            };
            let better = chosen.is_none_or(|current| {
                self.streams[current]
                    .buffer
                    .front()
                    .is_some_and(|best| head.path < best.path)
            });
            if better {
                chosen = Some(index);
            }
        }
        Ok(chosen.and_then(|index| self.streams[index].buffer.pop_front()))
    }
}
