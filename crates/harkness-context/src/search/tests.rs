//! The search contract, held to a repository rather than described.
//!
//! Four things are checked here that prose cannot promise: that two runs of one
//! query agree, that paging is exactly the unpaged answer, that nothing outside
//! the eligible inventory can ever match, and that every bound which fires says
//! so in the payload. The rest of the file is the edges — a pattern that will
//! not compile, a file that moved under the reader, a line longer than the
//! budget, a byte that is not UTF-8.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use harkness_core::ProjectId;
use harkness_git::Cancellation;
use harkness_test_fixtures::{Fixture, initialize_repository};

use super::{
    BoundedText, CursorRefusal, DEFAULT_MAX_RESULTS, MAX_LINE_BYTES_CAP, MAX_PATH_PREFIXES,
    MAX_PATTERN_BYTES, MAX_RESPONSE_BYTES_CAP, MAX_RESULTS_CAP, MAX_SEARCH_OMISSIONS, SearchCursor,
    SearchError, SearchFilters, SearchLimits, SearchOmission, SearchQuery, SearchResponse,
    TextEncoding,
};
use crate::classify::FileClass;
use crate::engine::{ContextEngine, ContextEngineConfig};
use crate::error::ContextEngineError;
use crate::index::WorktreeKey;
use crate::path::RepoPath;
use crate::provenance::RetrievalSource;

/// A repository, a data directory, and an engine over an index that is built
/// on demand.
struct Workspace {
    fixture: Fixture,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let fixture = Fixture::new();
        let root = fixture.directory("workspace");
        initialize_repository(&root);
        Self { fixture, root }
    }

    fn engine(&self) -> ContextEngine {
        ContextEngine::open(
            ContextEngineConfig::new(ProjectId::new(), &self.root, &self.fixture.data_dir),
            &Cancellation::default(),
        )
        .expect("the engine opens")
    }

    /// An engine whose index already describes everything written so far.
    fn indexed(&self) -> ContextEngine {
        let engine = self.engine();
        engine
            .reindex(&Cancellation::default())
            .expect("a cold build");
        engine
    }

    fn write(&self, relative: &str, body: impl AsRef<[u8]>) {
        let target = self.root.join(relative);
        fs::create_dir_all(target.parent().expect("a file has a parent")).unwrap();
        fs::write(&target, body).unwrap();
    }

    /// Writes and stamps a modification time a comparison can tell apart.
    ///
    /// Filesystem clocks are coarse enough that two writes inside one tick
    /// carry the same time, so a test about the *metadata* comparison stamps
    /// rather than hoping.
    fn write_stamped(&self, relative: &str, body: impl AsRef<[u8]>, epoch_seconds: u64) {
        self.write(relative, body);
        let times = fs::FileTimes::new()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_seconds));
        fs::File::options()
            .write(true)
            .open(self.root.join(relative))
            .unwrap()
            .set_times(times)
            .unwrap();
    }
}

/// Runs `query` and returns the paths and line numbers it found.
fn positions(response: &SearchResponse) -> Vec<(String, Option<u64>)> {
    response
        .matches
        .iter()
        .map(|found| (found.path.display(), found.line_number))
        .collect()
}

/// Every match of `query`, gathered one page at a time.
///
/// The paging is the point: a caller that follows the cursor must end up with
/// the answer an unbounded query would have given, in the same order, with
/// nothing repeated.
fn paged(engine: &ContextEngine, base: &SearchQuery, page: usize) -> Vec<(String, Option<u64>)> {
    let cancellation = Cancellation::default();
    let mut query = base
        .clone()
        .with_limits(SearchLimits::new().with_max_results(page));
    let mut gathered = Vec::new();
    // A repository this size cannot need more pages than it has matches, so the
    // bound is a runaway guard rather than a limit anything reaches.
    for _ in 0..1_000 {
        let response = engine.search(&query, &cancellation).expect("a page");
        gathered.extend(positions(&response));
        let Some(cursor) = response.next_cursor else {
            return gathered;
        };
        query = query.continuing(cursor);
    }
    panic!("paging did not terminate");
}

fn kind_of(error: &ContextEngineError) -> &'static str {
    error.kind()
}

fn search_error(error: ContextEngineError) -> SearchError {
    match error {
        ContextEngineError::Search(error) => error,
        other => panic!("expected a search refusal, found {other}"),
    }
}

// -- the vocabulary, with no repository in sight ----------------------------

#[test]
fn a_cursor_round_trips_through_its_opaque_token() {
    let cursor = SearchCursor::new(
        7,
        &WorktreeKey::for_root(Path::new("/w/checkout")),
        crate::digest::Sha256Hex::of(b"query"),
        // Deliberately not UTF-8: a path is a byte string, and a token that
        // spelled it through a lossy conversion would resume at a different
        // file than the page ended on.
        RepoPath::from_bytes(vec![b's', b'r', b'c', b'/', 0xff, 0xfe, b'.', b'r', b's']),
        4_096,
    );

    let token = cursor.token();
    let parsed = SearchCursor::parse(&token).expect("its own token");

    assert_eq!(parsed, cursor);
    assert_eq!(parsed.index_generation(), 7);
    assert_eq!(parsed.token(), token, "encoding is stable");
}

/// One repository's cache is shared by every linked worktree of it: the
/// generation is a single row and only the `files` rows are keyed by checkout.
/// A sibling's token therefore matches on generation *and* on query identity,
/// and still names a position in a different set of rows — so continuing from
/// it would silently skip everything the second checkout holds before that path.
#[test]
fn a_cursor_from_a_sibling_worktree_is_refused() {
    let identity = crate::digest::Sha256Hex::of(b"query");
    let here = WorktreeKey::for_root(Path::new("/w/one"));
    let sibling = WorktreeKey::for_root(Path::new("/w/two"));
    let cursor = SearchCursor::new(
        7,
        &sibling,
        identity.clone(),
        RepoPath::from_path(Path::new("src/z.rs")),
        900,
    );

    let refused = cursor.admits(7, &here, &identity).unwrap_err();

    assert_eq!(refused.kind(), "stale_search_cursor");
    assert!(
        matches!(
            refused,
            SearchError::StaleCursor {
                refusal: CursorRefusal::DifferentWorktree
            }
        ),
        "{refused}"
    );
    cursor
        .admits(7, &sibling, &identity)
        .expect("its own worktree");
}

#[test]
fn a_token_this_build_did_not_mint_is_refused_as_malformed() {
    for token in ["", "not-base64!!", "YWJj", "eyJ2Ijo5OTl9"] {
        let refused = SearchCursor::parse(token).unwrap_err();
        assert!(
            matches!(
                refused,
                SearchError::StaleCursor {
                    refusal: CursorRefusal::Malformed
                }
            ),
            "{token} produced {refused}"
        );
        assert_eq!(refused.kind(), "stale_search_cursor");
    }
}

#[test]
fn limits_are_clamped_rather_than_refused() {
    let zeroed = SearchLimits::new()
        .with_max_results(0)
        .with_max_bytes(0)
        .with_max_line_bytes(0);
    assert_eq!(zeroed.max_results(), DEFAULT_MAX_RESULTS);
    assert_eq!(zeroed.max_bytes(), super::DEFAULT_MAX_RESPONSE_BYTES);
    assert_eq!(zeroed.max_line_bytes(), super::DEFAULT_MAX_LINE_BYTES);
}

/// *Every* bound is capped, not merely defaulted. An uncapped `max_bytes` or
/// `max_line_bytes` would let a caller's number decide how much memory one
/// response holds — a thousand matches carrying eleven megabyte-long lines is
/// otherwise a single call away.
#[test]
fn no_caller_supplied_bound_can_exceed_its_published_cap() {
    let limits = SearchLimits::new()
        .with_max_results(usize::MAX)
        .with_context_lines(u32::MAX)
        .with_max_bytes(u64::MAX)
        .with_max_line_bytes(u64::MAX);

    assert_eq!(limits.max_results(), MAX_RESULTS_CAP);
    assert_eq!(limits.context_lines(), super::MAX_CONTEXT_LINES);
    assert_eq!(limits.max_bytes(), MAX_RESPONSE_BYTES_CAP);
    assert_eq!(limits.max_line_bytes(), MAX_LINE_BYTES_CAP);
}

/// Sorted *prefixes* do not imply sorted *paths*: `sr-x` sorts after `sr`
/// because `-` is above nothing in a prefix comparison, while `sr-x/a` sorts
/// *before* `sr/a` because `-` is below `/`. The merge exists for exactly this,
/// and normalization is what makes it a merge over disjoint streams.
#[test]
fn prefixes_are_normalized_into_a_sorted_disjoint_set() {
    let filters = SearchFilters::new()
        .under("src/inner")
        .unwrap()
        .under("sr-x")
        .unwrap()
        .under("src")
        .unwrap()
        .under("sr")
        .unwrap()
        .under("src")
        .unwrap();

    let prefixes: Vec<String> = filters.prefixes().iter().map(RepoPath::display).collect();
    assert_eq!(prefixes, vec!["sr", "sr-x", "src"], "{prefixes:?}");
}

/// A parent sorts before its children but is not *adjacent* to them: `src-gen`
/// falls between `src` and `src/inner`, because `-` is `0x2d` and the separator
/// is `0x2f`. Consulting only the previously kept prefix therefore keeps a
/// child its own parent already covers — and two overlapping streams emit one
/// file's matches twice, at the same `(path, byte_offset)`, which is the total
/// order a cursor has to be able to sit inside.
#[test]
fn a_child_prefix_is_dropped_even_when_a_sibling_sorts_between_it_and_its_parent() {
    let filters = SearchFilters::new()
        .under("src")
        .unwrap()
        .under("src-gen")
        .unwrap()
        .under("src/inner")
        .unwrap();

    let prefixes: Vec<String> = filters.prefixes().iter().map(RepoPath::display).collect();
    assert_eq!(prefixes, vec!["src", "src-gen"], "{prefixes:?}");
}

/// The same shape, through the engine: the overlap must not reach the merge.
#[test]
fn overlapping_prefixes_never_report_one_file_twice() {
    let workspace = Workspace::new();
    workspace.write("src/inner/a.rs", "needle\n");
    workspace.write("src-gen/b.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_filters(
                SearchFilters::new()
                    .under("src")
                    .unwrap()
                    .under("src-gen")
                    .unwrap()
                    .under("src/inner")
                    .unwrap(),
            ),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![
            ("src-gen/b.rs".to_owned(), Some(1)),
            ("src/inner/a.rs".to_owned(), Some(1)),
        ]
    );
    let mut previous: Option<(&RepoPath, u64)> = None;
    for found in &response.matches {
        if let Some(previous) = previous {
            assert!(previous < found.position(), "{previous:?} repeated");
        }
        previous = Some(found.position());
    }
}

/// A prefix is built from its *validated components*, not from the bytes a
/// caller typed. `Path` normalizes nothing, so a stored `src/` would range over
/// `src//`..`src/0` and match not one file in the subtree it names — and `.` is
/// a spelling of the worktree root rather than an escape from it.
#[test]
fn a_prefix_is_understood_rather_than_merely_accepted() {
    for spelling in ["", ".", "./"] {
        assert!(
            SearchFilters::new()
                .under(spelling)
                .unwrap()
                .prefixes()
                .is_empty(),
            "{spelling:?} names the worktree root and narrows nothing"
        );
    }

    for spelling in ["src", "src/", "src//", "./src", "src/./"] {
        let prefixes: Vec<String> = SearchFilters::new()
            .under(spelling)
            .unwrap()
            .prefixes()
            .iter()
            .map(RepoPath::display)
            .collect();
        assert_eq!(prefixes, vec!["src"], "{spelling:?}");
    }

    assert_eq!(
        SearchFilters::new()
            .under("a//b")
            .unwrap()
            .prefixes()
            .iter()
            .map(RepoPath::display)
            .collect::<Vec<_>>(),
        vec!["a/b"]
    );
}

/// The same, through the engine: a filter spelled with a trailing separator
/// must narrow to the subtree it plainly names rather than to nothing.
#[test]
fn a_prefix_with_a_trailing_separator_still_narrows_to_its_subtree() {
    let workspace = Workspace::new();
    workspace.write("src/inside.rs", "needle\n");
    workspace.write("elsewhere/outside.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_filters(SearchFilters::new().under("src/").unwrap()),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![("src/inside.rs".to_owned(), Some(1))]
    );
}

#[test]
fn a_prefix_leaving_the_workspace_is_refused_by_name() {
    for escape in ["../outside", "src/../../outside", "/etc"] {
        let refused = SearchFilters::new().under(escape).unwrap_err();
        assert_eq!(refused.kind(), "forbidden_path", "{escape} was allowed");
    }
    #[cfg(windows)]
    assert_eq!(
        SearchFilters::new()
            .under("C:\\Windows")
            .unwrap_err()
            .kind(),
        "forbidden_path"
    );
}

/// A size limit is not a containment failure. Telling somebody their
/// sixty-fifth ordinary `src/` filter left the workspace is the wrong answer to
/// the wrong question, so the two have separate discriminants.
#[test]
fn a_query_may_not_name_more_subtrees_than_the_merge_holds() {
    let mut filters = SearchFilters::new();
    for index in 0..MAX_PATH_PREFIXES {
        filters = filters.under(format!("dir-{index}")).unwrap();
    }
    let refused = filters.clone().under("one-too-many").unwrap_err();
    assert_eq!(refused.kind(), "too_many_filters");
    assert!(matches!(refused, SearchError::TooManyFilters { limit } if limit == MAX_PATH_PREFIXES));

    // The count is of *distinct subtrees*, so repeating one costs nothing and
    // a prefix already covered costs nothing either.
    filters
        .clone()
        .under("dir-0")
        .unwrap()
        .under("dir-0/nested")
        .unwrap();
}

#[test]
fn bounded_text_round_trips_and_never_cuts_a_character_in_half() {
    let plain = BoundedText::clamped(b"needle", 64);
    assert_eq!(plain.encoding(), TextEncoding::Utf8);
    assert_eq!(plain.as_str(), "needle");
    assert!(!plain.is_truncated());
    assert_eq!(plain.bytes(), b"needle");

    // "aéb" is four bytes and a limit of two lands inside the two-byte
    // character; the clamp walks back rather than emitting half of it.
    let cut = BoundedText::clamped("aéb".as_bytes(), 2);
    assert_eq!(cut.encoding(), TextEncoding::Utf8);
    assert_eq!(cut.as_str(), "a");
    assert!(cut.is_truncated());
    assert_eq!(cut.source_bytes(), 4);

    let arbitrary = BoundedText::clamped(&[0xff, 0xfe, b'a'], 64);
    assert_eq!(arbitrary.encoding(), TextEncoding::Base64);
    assert_eq!(arbitrary.bytes(), vec![0xff, 0xfe, b'a']);
}

#[test]
fn every_search_variant_maps_to_a_listed_kind_in_declaration_order() {
    let cases = [
        (
            SearchError::InvalidPattern {
                pattern_kind: "regex",
                reason: "unclosed group".to_owned(),
            },
            "invalid_pattern",
        ),
        (SearchError::RegexNotPermitted, "regex_not_permitted"),
        (
            SearchError::ForbiddenPath {
                path: "../outside".to_owned(),
                reason: "'..' would leave the workspace",
            },
            "forbidden_path",
        ),
        (
            SearchError::TooManyFilters {
                limit: MAX_PATH_PREFIXES,
            },
            "too_many_filters",
        ),
        (
            SearchError::StaleCursor {
                refusal: CursorRefusal::GenerationChanged,
            },
            "stale_search_cursor",
        ),
        (
            SearchError::IndexUnavailable {
                worktree: "key".to_owned(),
                reason: "no batch has ever published this checkout; build the index first",
            },
            "index_unavailable",
        ),
        (SearchError::Cancelled, "cancelled"),
    ];

    let kinds: Vec<&str> = cases.iter().map(|(_, kind)| *kind).collect();
    assert_eq!(kinds, SearchError::KINDS);
    for (error, expected) in cases {
        assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
    }
    // Every refusal a cursor can carry is reachable and spelled.
    let spellings: Vec<&str> = CursorRefusal::ALL.iter().map(|one| one.as_str()).collect();
    assert_eq!(
        spellings,
        vec![
            "malformed",
            "generation_changed",
            "different_query",
            "different_worktree"
        ]
    );
}

/// The omission kinds reach a payload and a reference table, and the type is
/// `#[non_exhaustive]` — so a caller matches on the string, and a variant added
/// with a duplicate or misspelled discriminant must fail here rather than ship.
#[test]
fn every_omission_variant_maps_to_a_listed_kind_in_declaration_order() {
    let path = RepoPath::from_path(Path::new("src/a.rs"));
    let cases = [
        (
            SearchOmission::ResultBudgetExhausted { limit: 1 },
            "result_budget_exhausted",
        ),
        (
            SearchOmission::ByteBudgetExhausted { limit: 1 },
            "byte_budget_exhausted",
        ),
        (
            SearchOmission::LineTooLong {
                path: path.clone(),
                byte_offset: 0,
                limit: 1,
            },
            "line_too_long",
        ),
        (
            SearchOmission::FileUnreadable { path: path.clone() },
            "file_unreadable",
        ),
        (
            SearchOmission::FileChangedSinceIndex { path: path.clone() },
            "file_changed_since_index",
        ),
        (
            SearchOmission::EncodingNotSearchable {
                path: path.clone(),
                encoding: crate::chunk::ContentEncoding::Utf16Le,
            },
            "encoding_not_searchable",
        ),
        (
            SearchOmission::BinaryContentDetected {
                path,
                byte_offset: 0,
            },
            "binary_content_detected",
        ),
    ];

    let kinds: Vec<&str> = cases.iter().map(|(_, kind)| *kind).collect();
    assert_eq!(kinds, SearchOmission::KINDS);
    for (omission, expected) in cases {
        assert_eq!(
            omission.kind(),
            expected,
            "unexpected kind for {omission:?}"
        );
    }

    let mut sorted = SearchOmission::KINDS.to_vec();
    sorted.sort_unstable();
    let count = sorted.len();
    sorted.dedup();
    assert_eq!(sorted.len(), count, "two omissions share a spelling");
}

/// A cursor is bound to everything that decides *which* matches exist and in
/// what order, and to nothing that decides how much arrives at a time.
///
/// The four bounds are all outside it. Page size obviously — a caller asking
/// for a smaller second page is paging. So are the two *text* bounds, which is
/// less obvious and is the same rule: a surface that lets somebody expand the
/// context around a result and keep paging is asking one question, not two.
#[test]
fn a_query_identity_covers_what_decides_its_matches_and_not_how_much_they_carry() {
    let base = SearchQuery::exact("needle");
    for bounded in [
        SearchLimits::new().with_max_results(3),
        SearchLimits::new().with_max_bytes(9),
        SearchLimits::new().with_context_lines(1),
        SearchLimits::new().with_max_line_bytes(16),
    ] {
        assert_eq!(
            base.identity(),
            base.clone().with_limits(bounded).identity(),
            "{bounded:?} does not change which matches exist"
        );
    }

    let variants = [
        SearchQuery::exact("other"),
        SearchQuery::regex("needle"),
        SearchQuery::filename("needle"),
        SearchQuery::exact("needle").permitting_regex(),
        SearchQuery::exact("needle").with_filters(SearchFilters::new().under("src").unwrap()),
        SearchQuery::exact("needle").with_filters(SearchFilters::new().in_class(FileClass::Source)),
    ];
    for variant in variants {
        assert_ne!(
            base.identity(),
            variant.identity(),
            "{variant:?} shares an identity with the base query"
        );
    }
}

/// The consequence, through the engine: a surface may widen the context around
/// a result and keep following the same cursor.
#[test]
fn context_may_be_widened_between_pages_without_restarting_the_query() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "one\nneedle\nthree\nneedle\nfive\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();

    let first = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_results(1)),
            &cancellation,
        )
        .unwrap();
    assert!(first.matches[0].before.is_empty());
    let cursor = first.next_cursor.clone().expect("a continuation");

    let widened = engine
        .search(
            &SearchQuery::exact("needle")
                .with_limits(SearchLimits::new().with_context_lines(1))
                .continuing(cursor),
            &cancellation,
        )
        .unwrap();

    assert_eq!(positions(&widened), vec![("src/a.rs".to_owned(), Some(4))]);
    assert_eq!(widened.matches[0].before[0].as_str(), "three");
}

// -- determinism and ordering ------------------------------------------------

/// The flagship claim: an unchanged worktree answers the same way twice.
///
/// The two *ids* differ, because they name the calls. Everything that names the
/// answer is equal, and the check is written so that the capture is the only
/// difference it will tolerate — a match's provenance is compared against its
/// own response's snapshot, so a field that started varying for any other
/// reason would fail here rather than be excused by a loose comparison.
#[test]
fn two_runs_of_one_query_over_an_unchanged_worktree_agree() {
    let workspace = Workspace::new();
    workspace.write(
        "src/alpha.rs",
        "let needle = 1;\nlet other = 2;\nneedle();\n",
    );
    workspace.write("src/beta.rs", "// needle\n");
    workspace.write("docs/needle.md", "nothing here\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();

    for query in [
        SearchQuery::exact("needle"),
        SearchQuery::regex("need[l]e").permitting_regex(),
        SearchQuery::filename("needle"),
    ] {
        let first = engine.search(&query, &cancellation).unwrap();
        let second = engine.search(&query, &cancellation).unwrap();

        let mut restamped = second.matches.clone();
        for found in &mut restamped {
            assert_eq!(
                found.provenance.snapshot_id, second.snapshot_id,
                "a match names the capture it was read under"
            );
            found.provenance.snapshot_id = first.snapshot_id;
        }
        assert_eq!(first.matches, restamped, "{query:?}");
        assert_eq!(first.omissions, second.omissions, "{query:?}");
        assert_eq!(first.stats, second.stats, "{query:?}");
        assert_eq!(first.next_cursor, second.next_cursor, "{query:?}");
        assert_ne!(
            first.query_id, second.query_id,
            "an id names the call, not the answer"
        );
        assert_ne!(first.snapshot_id, second.snapshot_id);
        assert!(!first.matches.is_empty(), "{query:?} found nothing");
    }
}

#[test]
fn matches_are_ordered_by_path_bytes_then_byte_offset() {
    let workspace = Workspace::new();
    workspace.write("src/b.rs", "needle\nneedle\n");
    workspace.write("src/a.rs", "x\nneedle\n");
    workspace.write("a.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![
            ("a.rs".to_owned(), Some(1)),
            ("src/a.rs".to_owned(), Some(2)),
            ("src/b.rs".to_owned(), Some(1)),
            ("src/b.rs".to_owned(), Some(2)),
        ]
    );
    // Strictly increasing: two matches sharing a position would be two a cursor
    // could not sit between.
    let mut previous: Option<(&RepoPath, u64)> = None;
    for found in &response.matches {
        if let Some(previous) = previous {
            assert!(previous < found.position(), "{previous:?}");
        }
        previous = Some(found.position());
    }
}

/// Several narrowed subtrees are one ordered answer, not one answer per
/// subtree. `sr-x/one.rs` sorts before `sr/one.rs` even though the prefix `sr`
/// sorts before `sr-x`, so a concatenation would be out of order here and a
/// merge is not.
#[test]
fn several_prefixes_merge_into_one_global_path_order() {
    let workspace = Workspace::new();
    workspace.write("sr/one.rs", "needle\n");
    workspace.write("sr-x/one.rs", "needle\n");
    workspace.write("src/one.rs", "needle\n");
    workspace.write("elsewhere/one.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_filters(
                SearchFilters::new()
                    .under("src")
                    .unwrap()
                    .under("sr")
                    .unwrap()
                    .under("sr-x")
                    .unwrap(),
            ),
            &Cancellation::default(),
        )
        .unwrap();

    let paths: Vec<String> = response
        .matches
        .iter()
        .map(|found| found.path.display())
        .collect();
    assert_eq!(paths, vec!["sr-x/one.rs", "sr/one.rs", "src/one.rs"]);
}

/// Containment requires the separator, so a prefix never picks up a sibling
/// whose name it merely begins.
#[test]
fn a_path_filter_never_reaches_a_sibling_it_is_a_prefix_of() {
    let workspace = Workspace::new();
    workspace.write("src/inside.rs", "needle\n");
    workspace.write("src-generated/outside.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_filters(SearchFilters::new().under("src").unwrap()),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![("src/inside.rs".to_owned(), Some(1))]
    );
}

#[test]
fn a_class_filter_admits_only_the_classes_it_names() {
    let workspace = Workspace::new();
    workspace.write("src/code.rs", "needle\n");
    workspace.write("docs/prose.md", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle")
                .with_filters(SearchFilters::new().in_class(FileClass::Source)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![("src/code.rs".to_owned(), Some(1))]
    );
}

// -- paging ------------------------------------------------------------------

/// The paged union is the unpaged answer, at every page size. This is the
/// property the whole cursor design exists to hold: no duplicate and no gap.
#[test]
fn paging_yields_exactly_the_unpaged_answer_at_every_page_size() {
    let workspace = Workspace::new();
    for file in 0..7 {
        let body: String = (0..5)
            .map(|line| {
                if line % 2 == 0 {
                    format!("needle {file} {line}\n")
                } else {
                    format!("quiet {file} {line}\n")
                }
            })
            .collect();
        workspace.write(&format!("src/module-{file}/file.rs", file = file), body);
    }
    let engine = workspace.indexed();

    let whole = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();
    let expected = positions(&whole);
    assert_eq!(expected.len(), 21, "seven files, three matching lines each");
    assert!(whole.next_cursor.is_none());

    for page in 1..=expected.len() + 1 {
        assert_eq!(
            paged(&engine, &SearchQuery::exact("needle"), page),
            expected,
            "page size {page}"
        );
    }
}

/// The same property for the shape whose cursor handling is *different*.
///
/// A continuation seeds the cursor's own row by name, because a content page
/// may have stopped in the middle of a file. A filename match holds one
/// position per file, so that seeded row is always one the previous page
/// already returned — and offering it again would end every page with the match
/// that starts the next.
#[test]
fn paging_a_filename_query_yields_exactly_the_unpaged_answer() {
    let workspace = Workspace::new();
    for name in ["needle-a", "needle-b", "needle-c", "needle-d"] {
        workspace.write(&format!("src/{name}.rs"), "quiet\n");
    }
    let engine = workspace.indexed();

    let whole = engine
        .search(&SearchQuery::filename("needle"), &Cancellation::default())
        .unwrap();
    let expected = positions(&whole);
    assert_eq!(expected.len(), 4);
    assert!(whole.next_cursor.is_none());

    for page in 1..=expected.len() + 1 {
        assert_eq!(
            paged(&engine, &SearchQuery::filename("needle"), page),
            expected,
            "page size {page}"
        );
    }
}

/// A page that fills exactly is a complete answer. Handing it a cursor would
/// make every complete answer look truncated, which is the same mistake
/// `IndexedPage` refuses on the read side.
#[test]
fn a_page_that_fills_exactly_carries_no_cursor() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\nneedle\n");
    let engine = workspace.indexed();

    let exact = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_results(2)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(exact.matches.len(), 2);
    assert!(exact.next_cursor.is_none());
    assert!(exact.omissions.is_empty(), "{:?}", exact.omissions);
}

#[test]
fn a_result_budget_reports_itself_and_hands_back_a_usable_cursor() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\nneedle\nneedle\n");
    let engine = workspace.indexed();

    let first = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_results(2)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(first.matches.len(), 2);
    assert!(
        first
            .omissions
            .contains(&SearchOmission::ResultBudgetExhausted { limit: 2 })
    );
    let cursor = first.next_cursor.clone().expect("a continuation");
    let second = engine
        .search(
            &SearchQuery::exact("needle")
                .with_limits(SearchLimits::new().with_max_results(2))
                .continuing(cursor),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(positions(&second), vec![("src/a.rs".to_owned(), Some(3))]);
    assert!(second.next_cursor.is_none());
}

/// The omission list is capped and the truncation notice is not part of that
/// cap. A page whose omissions filled up with unreadable files would otherwise
/// lose the one entry a caller reads as "there is more" — and, since the cursor
/// is emitted only when a bound fired, its continuation with it.
#[test]
fn a_full_omission_list_never_swallows_the_truncation_notice() {
    let workspace = Workspace::new();
    // Sorted before the matching file, so the omissions accumulate first.
    for index in 0..MAX_SEARCH_OMISSIONS + 44 {
        workspace.write(&format!("a-{index:04}.rs"), "quiet\n");
    }
    workspace.write("z.rs", "needle\nneedle\nneedle\n");
    let engine = workspace.indexed();
    for index in 0..MAX_SEARCH_OMISSIONS + 44 {
        fs::remove_file(workspace.root.join(format!("a-{index:04}.rs"))).unwrap();
    }

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_results(2)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(response.matches.len(), 2);
    assert_eq!(response.dropped_omissions, 44);
    assert_eq!(response.omissions.len(), MAX_SEARCH_OMISSIONS + 1);
    assert_eq!(
        response.omissions.last(),
        Some(&SearchOmission::ResultBudgetExhausted { limit: 2 })
    );
    let cursor = response.next_cursor.clone().expect("a continuation");
    let rest = engine
        .search(
            &SearchQuery::exact("needle")
                .with_limits(SearchLimits::new().with_max_results(2))
                .continuing(cursor),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(positions(&rest), vec![("z.rs".to_owned(), Some(3))]);
}

#[test]
fn a_byte_budget_reports_itself_and_stops_at_the_boundary() {
    let workspace = Workspace::new();
    workspace.write(
        "src/a.rs",
        "needle aaaaaaaaaa\nneedle bbbbbbbbbb\nneedle cccccccccc\n",
    );
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_bytes(20)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(response.matches.len(), 1, "one 17-byte line fits in 20");
    assert!(
        response
            .omissions
            .contains(&SearchOmission::ByteBudgetExhausted { limit: 20 })
    );
    let cursor = response.next_cursor.clone().expect("a continuation");
    let rest = engine
        .search(
            &SearchQuery::exact("needle").continuing(cursor),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(
        positions(&rest),
        vec![
            ("src/a.rs".to_owned(), Some(2)),
            ("src/a.rs".to_owned(), Some(3))
        ]
    );
}

/// A budget smaller than one match still returns that match. An empty page
/// carrying a cursor that points before the very match that did not fit is a
/// caller paging politely forever over nothing.
#[test]
fn a_budget_smaller_than_one_match_returns_that_match_anyway() {
    let workspace = Workspace::new();
    workspace.write(
        "src/a.rs",
        "needle aaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nneedle b\n",
    );
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_bytes(1)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(response.matches.len(), 1);
    assert!(response.next_cursor.is_some());
}

#[test]
fn a_cursor_from_a_rebuilt_index_is_refused_rather_than_mixed() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\nneedle\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();
    let cursor = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_results(1)),
            &cancellation,
        )
        .unwrap()
        .next_cursor
        .expect("a continuation");

    // Disposing the cache is the supported "fix a weird index" action, and it
    // mints a new generation — every chunk reference taken against the old one
    // is now about a walk that no longer exists.
    engine.dispose_index(&cancellation).unwrap();
    engine.reindex(&cancellation).unwrap();

    let refused = engine
        .search(
            &SearchQuery::exact("needle").continuing(cursor),
            &cancellation,
        )
        .map(|_| ())
        .unwrap_err();

    assert_eq!(kind_of(&refused), "stale_search_cursor");
    assert!(matches!(
        search_error(refused),
        SearchError::StaleCursor {
            refusal: CursorRefusal::GenerationChanged
        }
    ));
}

#[test]
fn a_cursor_replayed_against_a_different_query_is_refused() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\nneedle\nhaystack\n");
    let engine = workspace.indexed();
    let cursor = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_results(1)),
            &Cancellation::default(),
        )
        .unwrap()
        .next_cursor
        .expect("a continuation");

    let refused = engine
        .search(
            &SearchQuery::exact("haystack").continuing(cursor),
            &Cancellation::default(),
        )
        .map(|_| ())
        .unwrap_err();

    assert!(matches!(
        search_error(refused),
        SearchError::StaleCursor {
            refusal: CursorRefusal::DifferentQuery
        }
    ));
}

// -- what may never match ----------------------------------------------------

/// Exclusion is by construction: a denied path, a secret-classified file, an
/// ignored one and a binary are not rows to be filtered but rows that were
/// never written. The pattern is present in every one of them.
#[test]
fn a_secret_an_ignored_and_a_binary_file_can_never_match() {
    let workspace = Workspace::new();
    workspace.write(".env", "API_TOKEN=needle\n");
    workspace.write("deploy.pem", "needle\n");
    workspace.write(".gitignore", "generated/\n");
    workspace.write("generated/output.rs", "needle\n");
    workspace.write("assets/blob.bin", b"needle\x00\x01\x02needle\n");
    workspace.write("src/visible.rs", "needle\n");
    let engine = workspace.indexed();

    for query in [SearchQuery::exact("needle"), SearchQuery::filename("env")] {
        let response = engine.search(&query, &Cancellation::default()).unwrap();
        for found in &response.matches {
            let path = found.path.display();
            assert!(
                !path.contains(".env")
                    && !path.contains("deploy.pem")
                    && !path.starts_with("generated/")
                    && !path.contains("blob.bin"),
                "{path} reached a result for {query:?}"
            );
        }
        // And the excluded content is not reachable through the omissions
        // either: a denied path is a count and never a name.
        let rendered = format!("{:?}", response.omissions);
        assert!(!rendered.contains(".env"), "{rendered}");
    }

    let visible = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();
    assert_eq!(
        positions(&visible),
        vec![("src/visible.rs".to_owned(), Some(1))],
        "the pattern really is present, so the exclusions are what removed the rest"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_pointing_outside_the_workspace_is_never_searched() {
    use std::os::unix::fs::symlink;

    let workspace = Workspace::new();
    let outside = workspace.fixture.root.path().join("outside.txt");
    fs::write(&outside, "needle\n").unwrap();
    symlink(&outside, workspace.root.join("link.txt")).unwrap();
    workspace.write("src/visible.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![("src/visible.rs".to_owned(), Some(1))],
        "a link is recorded and never followed"
    );
}

// -- patterns and capabilities -----------------------------------------------

#[test]
fn a_regex_query_is_refused_without_the_capability_and_runs_with_it() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "fn alpha() {}\nfn beta() {}\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();

    let refused = engine
        .search(&SearchQuery::regex("fn [ab]"), &cancellation)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(kind_of(&refused), "regex_not_permitted");

    let allowed = engine
        .search(
            &SearchQuery::regex("fn [ab]").permitting_regex(),
            &cancellation,
        )
        .unwrap();
    assert_eq!(allowed.matches.len(), 2);
}

/// An exact pattern is escaped, so nothing a caller writes is interpreted —
/// which is why it needs no capability at all.
#[test]
fn an_exact_pattern_matches_its_metacharacters_literally() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "a.b\naxb\n");
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("a.b"), &Cancellation::default())
        .unwrap();

    assert_eq!(positions(&response), vec![("src/a.rs".to_owned(), Some(1))]);
}

#[test]
fn a_pattern_that_cannot_be_run_is_refused_before_a_file_is_opened() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();

    let cases = [
        SearchQuery::exact(""),
        SearchQuery::filename(""),
        SearchQuery::exact("x".repeat(MAX_PATTERN_BYTES + 1)),
        // Unbalanced, so the engine cannot compile it.
        SearchQuery::regex("fn (").permitting_regex(),
        // A line-oriented scan may not be handed a pattern that spans lines.
        SearchQuery::regex("a\\nb").permitting_regex(),
        // Binary detection stops at a NUL, so a pattern that only matches one
        // would match content the scan never reaches.
        SearchQuery::regex("a\\x00b").permitting_regex(),
    ];
    for query in cases {
        let refused = engine
            .search(&query, &cancellation)
            .map(|_| ())
            .unwrap_err();
        assert_eq!(
            kind_of(&refused),
            "invalid_pattern",
            "{query:?} was accepted"
        );
    }
}

/// "Before a file is opened" is the weaker half of the promise. The capture
/// that stamps an answer reads the *whole workspace* and costs several times
/// what the scan does, so a refusable query must not reach one — otherwise
/// repeating a query that will always be refused is an unbounded workspace-read
/// amplifier for whatever calls this.
///
/// Proved by making the capture impossible: the worktree is removed, so any
/// call that reaches `snapshot` fails with the capture's own discriminant
/// rather than with the query's.
#[test]
fn a_refusable_query_never_reaches_the_capture() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();
    // Enough of the checkout to make a capture fail, while the cache the plan
    // is validated against is untouched.
    fs::remove_dir_all(workspace.root.join(".git")).unwrap();

    for query in [
        SearchQuery::exact(""),
        SearchQuery::regex("fn (").permitting_regex(),
        SearchQuery::regex("anything"),
    ] {
        let refused = engine
            .search(&query, &cancellation)
            .map(|_| ())
            .unwrap_err();
        assert!(
            matches!(refused.kind(), "invalid_pattern" | "regex_not_permitted"),
            "{query:?} answered {} — it reached the capture",
            refused.kind()
        );
    }

    // And the control: a query that *can* run does reach the capture, and fails
    // there. Without this the assertions above would also pass if `search` had
    // simply stopped working.
    let capture_failure = engine
        .search(&SearchQuery::exact("needle"), &cancellation)
        .map(|_| ())
        .unwrap_err();
    assert!(
        !matches!(
            capture_failure.kind(),
            "invalid_pattern" | "regex_not_permitted"
        ),
        "{capture_failure}"
    );
}

/// An unindexed worktree is decided the same way, and for the same reason.
#[test]
fn an_unindexed_worktree_is_refused_before_the_capture() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    fs::remove_dir_all(workspace.root.join(".git")).unwrap();

    let refused = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .map(|_| ())
        .unwrap_err();

    assert_eq!(kind_of(&refused), "index_unavailable");
}

/// A refusal carries the engine's own message about the caller's pattern,
/// because a person fixing a regular expression needs to know what was wrong
/// with theirs. What it must never carry is the pattern into a *log*, which is
/// the span's rule rather than this one's.
#[test]
fn an_invalid_pattern_refusal_explains_itself() {
    let workspace = Workspace::new();
    let engine = workspace.indexed();

    let refused = search_error(
        engine
            .search(
                &SearchQuery::regex("fn (").permitting_regex(),
                &Cancellation::default(),
            )
            .map(|_| ())
            .unwrap_err(),
    );

    match refused {
        SearchError::InvalidPattern {
            pattern_kind,
            reason,
        } => {
            assert_eq!(pattern_kind, "regex");
            assert!(!reason.is_empty());
        }
        other => panic!("expected an invalid pattern, found {other}"),
    }
}

// -- honesty about what was not returned -------------------------------------

#[test]
fn an_empty_result_is_a_success_with_no_omissions() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "nothing to find\n");
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    assert!(response.matches.is_empty());
    assert!(response.omissions.is_empty());
    assert_eq!(response.dropped_omissions, 0);
    assert!(response.next_cursor.is_none());
    assert!(response.stats.paths_examined > 0, "it really did look");
}

/// Context lines are clamped by the same bound as the match line, so the bound
/// fires on them too — and a bound that fired with nothing in the payload
/// saying so is the one thing the omission list exists to prevent.
#[test]
fn a_clamped_context_line_reports_itself_even_when_the_match_line_fits() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", format!("{}\nneedle\n", "x".repeat(200)));
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(
                SearchLimits::new()
                    .with_context_lines(1)
                    .with_max_line_bytes(16),
            ),
            &Cancellation::default(),
        )
        .unwrap();

    let found = &response.matches[0];
    assert!(!found.line.is_truncated(), "the match line itself fits");
    assert!(found.before[0].is_truncated());
    assert!(
        response.omissions.contains(&SearchOmission::LineTooLong {
            path: RepoPath::from_path(Path::new("src/a.rs")),
            byte_offset: 201,
            limit: 16,
        }),
        "{:?}",
        response.omissions
    );
}

#[test]
fn a_long_line_is_clamped_and_the_omission_says_which_position() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", format!("needle {}\n", "x".repeat(200)));
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_max_line_bytes(16)),
            &Cancellation::default(),
        )
        .unwrap();

    let found = &response.matches[0];
    assert!(found.line.is_truncated());
    assert_eq!(found.line.as_str().len(), 16);
    assert_eq!(found.line.source_bytes(), 207);
    assert!(found.provenance.truncated);
    assert!(
        response.omissions.contains(&SearchOmission::LineTooLong {
            path: RepoPath::from_path(Path::new("src/a.rs")),
            byte_offset: 0,
            limit: 16,
        }),
        "{:?}",
        response.omissions
    );
    // The range names exactly the bytes the digest covers — the *shown* prefix
    // and not the whole line — because reading the range back out of the file
    // and hashing it is the one check a range exists for.
    let range = found.provenance.range.expect("a byte range");
    assert_eq!(range.start, 0);
    assert_eq!(range.end, 16, "the emitted prefix, not the 207-byte line");
    assert_eq!(
        found.provenance.content_sha256,
        crate::digest::Sha256Hex::of(found.line.bytes())
    );
}

/// The general form of that check, over a match that was *not* clamped: read
/// the range out of the file, hash it, and it is the digest provenance carries.
#[test]
fn a_range_and_its_digest_describe_the_same_bytes() {
    let workspace = Workspace::new();
    let body = "one\nlet needle = 1;\nthree\n";
    workspace.write("src/a.rs", body);
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    let found = &response.matches[0];
    let range = found.provenance.range.expect("a byte range");
    let start = usize::try_from(range.start).unwrap();
    let end = usize::try_from(range.end).unwrap();
    assert_eq!(&body.as_bytes()[start..end], b"let needle = 1;");
    assert_eq!(
        found.provenance.content_sha256,
        crate::digest::Sha256Hex::of(&body.as_bytes()[start..end])
    );
    assert!(!found.provenance.truncated);
}

/// Binary detection abandons the *whole* file, not the tail of it — even where
/// the NUL sits after content that would have matched.
///
/// That is the safe direction, and it is why the fact is an omission rather
/// than a silent filter: the alternative is a caller reading "no match" from a
/// file nobody looked inside. Pinned here because it is the searcher's
/// behaviour rather than this module's, and a version of it that reported the
/// prefix instead would change an answer without changing a line of this crate.
#[test]
fn binary_content_abandons_the_whole_file_and_says_so() {
    let workspace = Workspace::new();
    // Indexed as text, then rewritten with a NUL *after* a matching line.
    workspace.write_stamped("src/a.rs", "needle\nquiet\n", 1_000_000);
    workspace.write_stamped("src/b.rs", "needle\n", 1_000_000);
    let engine = workspace.indexed();
    let mut rewritten = b"needle\n".to_vec();
    rewritten.extend_from_slice(&[0x00, 0xde, 0xad, b'\n']);
    workspace.write_stamped("src/a.rs", rewritten, 2_000_000);

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_context_lines(5)),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(
        positions(&response),
        vec![("src/b.rs".to_owned(), Some(1))],
        "the match before the NUL is abandoned with the rest of the file"
    );
    assert!(
        response.omissions.iter().any(|omission| matches!(
            omission,
            SearchOmission::BinaryContentDetected { path, byte_offset }
                if path.display() == "src/a.rs" && *byte_offset == 7
        )),
        "{:?}",
        response.omissions
    );
}

/// Search reads the working tree, and the working tree is allowed to hold bytes
/// no encoding this build decodes. The file was indexed as ordinary UTF-8
/// source and then rewritten, which is the reachable route to a match line that
/// cannot be spelled as a string — and the excerpt has to be honest about it
/// rather than lossily converting a byte a caller may need back.
#[test]
fn non_utf8_match_content_is_base64_and_reconstructs_the_exact_bytes() {
    let workspace = Workspace::new();
    workspace.write_stamped("src/latin.rs", "let clean = 1;\n", 1_000_000);
    let engine = workspace.indexed();
    workspace.write_stamped("src/latin.rs", b"needle caf\xe9\n", 2_000_000);

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    let found = &response.matches[0];
    assert_eq!(found.line.encoding(), TextEncoding::Base64);
    assert_eq!(found.line.bytes(), b"needle caf\xe9");
    assert!(!found.line.is_truncated());
    assert_eq!(found.line.source_bytes(), 11);
    assert_eq!(
        found.provenance.content_sha256,
        crate::digest::Sha256Hex::of(b"needle caf\xe9")
    );
}

/// The other route to bytes that are not text, and the one that needs no file
/// to have moved: a repository-relative path need not be UTF-8 either, and a
/// filename match shows the path.
#[cfg(unix)]
#[test]
fn a_filename_match_on_a_path_that_is_not_utf8_carries_its_exact_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let workspace = Workspace::new();
    let name = OsStr::from_bytes(b"needle-\xff.rs");
    fs::write(workspace.root.join("src").join(name), "quiet\n").unwrap_or_else(|_| {
        fs::create_dir_all(workspace.root.join("src")).unwrap();
        fs::write(workspace.root.join("src").join(name), "quiet\n").unwrap();
    });
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::filename("needle"), &Cancellation::default())
        .unwrap();

    let found = &response.matches[0];
    assert_eq!(found.line.encoding(), TextEncoding::Base64);
    assert_eq!(found.line.bytes(), b"src/needle-\xff.rs");
    assert!(found.path.is_lossy());
}

#[test]
fn a_utf16_file_is_reported_rather_than_silently_unsearched() {
    let workspace = Workspace::new();
    let mut body = vec![0xff, 0xfe];
    for unit in "needle\n".encode_utf16() {
        body.extend_from_slice(&unit.to_le_bytes());
    }
    workspace.write("src/wide.rs", body);
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    assert!(response.matches.is_empty());
    assert!(
        response.omissions.iter().any(|omission| matches!(
            omission,
            SearchOmission::EncodingNotSearchable { path, .. }
                if path.display() == "src/wide.rs"
        )),
        "{:?}",
        response.omissions
    );
}

#[test]
fn a_file_that_moved_since_indexing_is_searched_as_it_is_and_says_so() {
    let workspace = Workspace::new();
    workspace.write_stamped("src/a.rs", "old content\n", 1_000_000);
    let engine = workspace.indexed();
    let indexed_digest = engine
        .indexed_file(&RepoPath::from_path(Path::new("src/a.rs")))
        .unwrap()
        .and_then(|row| row.content_sha256)
        .expect("a digest");

    workspace.write_stamped("src/a.rs", "needle now\n", 2_000_000);

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    assert_eq!(positions(&response), vec![("src/a.rs".to_owned(), Some(1))]);
    assert!(
        response
            .omissions
            .contains(&SearchOmission::FileChangedSinceIndex {
                path: RepoPath::from_path(Path::new("src/a.rs")),
            }),
        "{:?}",
        response.omissions
    );
    let stamped = response.matches[0]
        .content_sha256
        .clone()
        .expect("the digest of what was read");
    assert_ne!(
        stamped, indexed_digest,
        "provenance names the bytes that were searched"
    );
    assert_eq!(stamped, crate::digest::Sha256Hex::of(b"needle now\n"));
}

#[test]
fn a_file_deleted_since_indexing_is_reported_and_the_scan_continues() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\n");
    workspace.write("src/b.rs", "needle\n");
    let engine = workspace.indexed();
    fs::remove_file(workspace.root.join("src/a.rs")).unwrap();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    assert_eq!(positions(&response), vec![("src/b.rs".to_owned(), Some(1))]);
    assert!(
        response
            .omissions
            .contains(&SearchOmission::FileUnreadable {
                path: RepoPath::from_path(Path::new("src/a.rs")),
            }),
        "{:?}",
        response.omissions
    );
}

// -- which capture a search is stamped with -----------------------------------

/// The two spellings answer identically; the only difference is which capture
/// the matches are stamped with, and a run passes its own so its evidence
/// describes one moment rather than one per query.
#[test]
fn a_search_under_a_held_capture_answers_exactly_as_one_that_takes_its_own() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\nneedle\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();
    let snapshot = engine.snapshot(&cancellation).unwrap();

    let taken = engine
        .search(&SearchQuery::exact("needle"), &cancellation)
        .unwrap();
    let given = engine
        .search_under(&snapshot, &SearchQuery::exact("needle"), &cancellation)
        .unwrap();

    assert_eq!(positions(&taken), positions(&given));
    assert_eq!(taken.stats, given.stats);
    assert_eq!(given.snapshot_id, snapshot.id());
    assert_ne!(taken.snapshot_id, given.snapshot_id);
    for found in &given.matches {
        assert_eq!(found.provenance.snapshot_id, snapshot.id());
    }
}

/// A capture of another checkout would build provenance that is well formed and
/// false, so it is refused rather than used.
#[test]
fn a_capture_of_another_checkout_is_refused_rather_than_stamped_onto_results() {
    let searched = Workspace::new();
    searched.write("src/a.rs", "needle\n");
    let engine = searched.indexed();
    let elsewhere = Workspace::new();
    let foreign = elsewhere
        .engine()
        .snapshot(&Cancellation::default())
        .unwrap();

    let refused = engine
        .search_under(
            &foreign,
            &SearchQuery::exact("needle"),
            &Cancellation::default(),
        )
        .map(|_| ())
        .unwrap_err();

    assert_eq!(kind_of(&refused), "foreign_snapshot");
    assert!(matches!(
        refused,
        ContextEngineError::ForeignSnapshot { .. }
    ));
}

// -- context, provenance, and cancellation ------------------------------------

#[test]
fn context_lines_come_from_the_file_and_stop_at_its_edges() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "one\ntwo\nneedle\nfour\nfive\n");
    workspace.write("src/edge.rs", "needle\n");
    let engine = workspace.indexed();

    let response = engine
        .search(
            &SearchQuery::exact("needle").with_limits(SearchLimits::new().with_context_lines(2)),
            &Cancellation::default(),
        )
        .unwrap();

    let middle = &response.matches[0];
    assert_eq!(middle.path.display(), "src/a.rs");
    let before: Vec<&str> = middle.before.iter().map(BoundedText::as_str).collect();
    let after: Vec<&str> = middle.after.iter().map(BoundedText::as_str).collect();
    assert_eq!(before, vec!["one", "two"], "in file order");
    assert_eq!(after, vec!["four", "five"]);

    let edge = &response.matches[1];
    assert_eq!(edge.path.display(), "src/edge.rs");
    assert!(edge.before.is_empty());
    assert!(edge.after.is_empty());
}

#[test]
fn every_match_carries_the_provenance_a_later_reader_needs() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "x\nlet needle = 1;\n");
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::exact("needle"), &Cancellation::default())
        .unwrap();

    let found = &response.matches[0];
    let provenance = &found.provenance;
    assert_eq!(provenance.source, RetrievalSource::LexicalSearch);
    assert_eq!(
        provenance.path.as_ref().map(RepoPath::display),
        Some("src/a.rs".to_owned())
    );
    assert_eq!(provenance.snapshot_id, response.snapshot_id);
    let range = provenance.range.expect("a byte range");
    assert_eq!(range.start, 2, "the second line starts after 'x\\n'");
    assert_eq!(range.end, 2 + "let needle = 1;".len() as u64);
    assert_eq!(range.first_line, Some(2));
    assert_eq!(range.last_line, Some(2));
    assert_eq!(found.byte_offset, 2 + "let ".len() as u64);
    // The provenance digest covers what was *shown*; the match's own covers the
    // file version that was searched. They are different questions.
    assert_eq!(
        provenance.content_sha256,
        crate::digest::Sha256Hex::of(b"let needle = 1;")
    );
    assert_eq!(
        found.content_sha256,
        Some(crate::digest::Sha256Hex::of(b"x\nlet needle = 1;\n"))
    );
    assert!(!provenance.truncated);
}

#[test]
fn a_filename_match_reads_no_content_and_is_attributed_to_the_path_search() {
    let workspace = Workspace::new();
    workspace.write("src/needle_finder.rs", "nothing to find\n");
    let engine = workspace.indexed();

    let response = engine
        .search(&SearchQuery::filename("needle"), &Cancellation::default())
        .unwrap();

    let found = &response.matches[0];
    assert_eq!(found.provenance.source, RetrievalSource::FilenameSearch);
    assert_eq!(found.line.as_str(), "src/needle_finder.rs");
    assert_eq!(found.line_number, None);
    assert_eq!(found.byte_offset, 0);
    assert_eq!(found.content_sha256, None);
    assert_eq!(response.stats.files_scanned, 0);
    assert_eq!(response.stats.bytes_read, 0);
}

#[test]
fn a_cancelled_search_yields_no_partial_page() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "needle\n");
    let engine = workspace.indexed();
    let cancellation = Cancellation::default();
    cancellation.cancel();

    let refused = engine
        .search(&SearchQuery::exact("needle"), &cancellation)
        .map(|_| ())
        .unwrap_err();

    assert_eq!(kind_of(&refused), "cancelled");
    assert!(matches!(refused, ContextEngineError::Cancelled));
}

// -- latency ------------------------------------------------------------------

/// The medium profile the milestone budgets: about ten thousand eligible files.
const MEDIUM_FILES: usize = 10_000;

/// Builds a medium repository and its warm index.
///
/// The bodies are sized like ordinary source rather than like the inventory
/// benchmark's, because what this measures is the scan and the per-file open
/// rather than the classifier's eight-kilobyte window.
///
/// Committed, and that is not incidental. A search captures a snapshot, and an
/// uncommitted tree makes the capture hash every file in it — so a benchmark
/// over ten thousand untracked files would measure the probe rather than the
/// scan, and would measure it in a state no repository stays in.
fn medium_workspace() -> (Workspace, ContextEngine) {
    let workspace = Workspace::new();
    let repository = harkness_git::git2::Repository::open(&workspace.root).unwrap();
    let body = "fn helper(value: u32) -> u32 { value + 1 }\n".repeat(24);
    for index in 0..MEDIUM_FILES {
        workspace.write(
            &format!("src/module-{}/file-{index}.rs", index % 100),
            &body,
        );
    }
    harkness_test_fixtures::commit_all(&repository, "a medium repository");
    let engine = workspace.indexed();
    (workspace, engine)
}

/// Both targets measure [`ContextEngine::search_under`], with the capture taken
/// once outside the timed loop.
///
/// That is the arrangement a caller gets, not a convenient one: a run records
/// one workspace snapshot and every retrieval it makes is stamped with that
/// one, so the capture is a cost the run has already paid before the first
/// query. Timing it again per query would measure the same workspace read once
/// per search and report a budget nobody pays. The capture is printed beside
/// the numbers anyway, so its size is on the record rather than hidden by the
/// choice. It is the same reason the inventory walk's target captures outside
/// its loop.
#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn a_medium_repository_meets_the_content_search_latency_target() {
    const RUNS: usize = 5;

    let (_workspace, engine) = medium_workspace();
    let cancellation = Cancellation::default();
    let capture = std::time::Instant::now();
    let snapshot = engine.snapshot(&cancellation).unwrap();
    println!(
        "the capture this measurement excludes: {:?}",
        capture.elapsed()
    );

    // A pattern *no* file holds, which is the worst case rather than the
    // convenient one: a query that matches early stops at its result budget
    // after a handful of files and measures almost nothing. This one opens and
    // scans every eligible file in the repository before it can answer.
    let exhaustive = SearchQuery::exact("no file in this repository holds this");
    // Beside it, the arrangement a caller usually meets: a pattern in every
    // file, stopped by the default result budget.
    let budgeted = SearchQuery::exact("fn helper");

    let mut timings = Vec::with_capacity(RUNS);
    let mut budgeted_timings = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = std::time::Instant::now();
        let response = engine
            .search_under(&snapshot, &exhaustive, &cancellation)
            .unwrap();
        timings.push(started.elapsed());
        assert!(response.matches.is_empty());
        assert!(response.stats.files_scanned >= MEDIUM_FILES as u64);

        let started = std::time::Instant::now();
        let response = engine
            .search_under(&snapshot, &budgeted, &cancellation)
            .unwrap();
        budgeted_timings.push(started.elapsed());
        assert_eq!(response.matches.len(), DEFAULT_MAX_RESULTS);
    }
    timings.sort_unstable();
    budgeted_timings.sort_unstable();
    println!("exhaustive content search over {MEDIUM_FILES} files: {timings:?}");
    println!("budgeted content search over {MEDIUM_FILES} files: {budgeted_timings:?}");
    harkness_test_fixtures::latency::record(
        "context::lexical_search_of_a_medium_repository",
        *timings.last().unwrap(),
        Duration::from_millis(100),
    );
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn a_medium_repository_meets_the_filename_search_latency_target() {
    const RUNS: usize = 5;

    let (_workspace, engine) = medium_workspace();
    let cancellation = Cancellation::default();
    let capture = std::time::Instant::now();
    let snapshot = engine.snapshot(&cancellation).unwrap();
    println!(
        "the capture this measurement excludes: {:?}",
        capture.elapsed()
    );
    // The whole path table, filtered to one entry: a substring nothing shares a
    // prefix with still visits every row, which is what the budget is about.
    let query = SearchQuery::filename("file-9999.rs");

    let mut timings = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = std::time::Instant::now();
        let response = engine
            .search_under(&snapshot, &query, &cancellation)
            .unwrap();
        timings.push(started.elapsed());
        assert_eq!(response.matches.len(), 1);
        assert_eq!(response.stats.files_scanned, 0);
        assert!(response.stats.paths_examined >= MEDIUM_FILES as u64);
    }
    timings.sort_unstable();
    println!("filename search over {MEDIUM_FILES} files: {timings:?}");
    harkness_test_fixtures::latency::record(
        "context::filename_search_of_a_medium_repository",
        *timings.last().unwrap(),
        Duration::from_millis(25),
    );
}
