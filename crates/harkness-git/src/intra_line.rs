//! Deterministic, byte-oriented pairing for replacement lines.
//!
//! This is deliberately private policy behind the public diff model. Front
//! ends consume the resulting counterpart indices and byte ranges; they never
//! repeat the pairing or token diff themselves.

use std::mem;

use super::diff::{
    DiffLine, DiffLineKind, Hunk, IntraLineDegradation, IntraLineRange, MAX_INTRA_LINE_BYTES,
    MAX_INTRA_LINE_COMPARISONS,
};

/// Lines sharing less than one fifth of their outer bytes are left unpaired.
/// A lone deletion/addition pair is the exception: a complete rewrite is
/// useful as one deterministic pair carrying full-line ranges.
const MIN_PAIR_SIMILARITY_PER_MILLE: usize = 200;

#[derive(Debug)]
struct ChangedRun {
    deletions: Vec<usize>,
    additions: Vec<usize>,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    start: usize,
    end: usize,
}

/// Adds pair and range metadata to one already-collected hunk.
///
/// The ordinary hunk bytes are never touched. Every bound is preflighted
/// before metadata is applied, so a degraded hunk contains no partial answer.
pub(super) fn annotate(hunk: &mut Hunk) {
    hunk.intra_line_degradation = None;
    for line in &mut hunk.lines {
        line.paired_line_index = None;
        line.intra_line_ranges = None;
    }

    let runs = changed_runs(&hunk.lines);
    let pairable = runs
        .iter()
        .filter(|run| !run.deletions.is_empty() && !run.additions.is_empty())
        .collect::<Vec<_>>();
    if pairable.is_empty() {
        return;
    }

    if pairable.iter().any(|run| {
        run.deletions
            .iter()
            .chain(&run.additions)
            .any(|index| hunk.lines[*index].content.len() > MAX_INTRA_LINE_BYTES)
    }) {
        hunk.intra_line_degradation = Some(IntraLineDegradation::LineTooLong {
            limit: MAX_INTRA_LINE_BYTES,
        });
        return;
    }

    // `line_similarity` can inspect each candidate's bytes from both ends.
    // Account for that worst case before doing any alignment work.
    let mut comparisons = 0usize;
    for run in &pairable {
        let deletion_bytes = run.deletions.iter().fold(0usize, |total, index| {
            total.saturating_add(hunk.lines[*index].content.len())
        });
        let addition_bytes = run.additions.iter().fold(0usize, |total, index| {
            total.saturating_add(hunk.lines[*index].content.len())
        });
        let candidates = run.deletions.len().saturating_mul(run.additions.len());
        let scanned_bytes = run
            .additions
            .len()
            .saturating_mul(deletion_bytes)
            .saturating_add(run.deletions.len().saturating_mul(addition_bytes))
            .saturating_mul(2);
        comparisons = comparisons
            .saturating_add(candidates)
            .saturating_add(scanned_bytes);
        if comparisons > MAX_INTRA_LINE_COMPARISONS {
            degrade_pairing(hunk);
            return;
        }
    }

    let mut pairs = Vec::new();
    for run in pairable {
        pairs.extend(pair_run(run, &hunk.lines));
    }

    let mut annotations = Vec::with_capacity(pairs.len());
    for (deletion, addition) in pairs {
        let old_content = &hunk.lines[deletion].content;
        let new_content = &hunk.lines[addition].content;
        if line_similarity(old_content, new_content) == 0 {
            annotations.push((
                deletion,
                addition,
                full_range(old_content),
                full_range(new_content),
            ));
            continue;
        }
        let old_tokens = tokenize(old_content);
        let new_tokens = tokenize(new_content);
        let range_cells = old_tokens
            .len()
            .saturating_add(1)
            .saturating_mul(new_tokens.len().saturating_add(1));
        comparisons = comparisons.saturating_add(range_cells);
        if comparisons > MAX_INTRA_LINE_COMPARISONS {
            degrade_pairing(hunk);
            return;
        }
        let (old_ranges, new_ranges) =
            changed_token_ranges(old_content, &old_tokens, new_content, &new_tokens);
        annotations.push((deletion, addition, old_ranges, new_ranges));
    }

    for (deletion, addition, old_ranges, new_ranges) in annotations {
        hunk.lines[deletion].paired_line_index = Some(addition);
        hunk.lines[deletion].intra_line_ranges = Some(old_ranges);
        hunk.lines[addition].paired_line_index = Some(deletion);
        hunk.lines[addition].intra_line_ranges = Some(new_ranges);
    }
}

fn full_range(content: &[u8]) -> Vec<IntraLineRange> {
    if content.is_empty() {
        Vec::new()
    } else {
        vec![IntraLineRange {
            start: 0,
            end: content.len(),
        }]
    }
}

fn degrade_pairing(hunk: &mut Hunk) {
    hunk.intra_line_degradation = Some(IntraLineDegradation::PairingTooLarge {
        limit: MAX_INTRA_LINE_COMPARISONS,
    });
}

/// Splits a hunk at context lines while allowing no-newline markers to remain
/// inside their replacement run. Pure insertion/deletion runs need no pairing.
fn changed_runs(lines: &[DiffLine]) -> Vec<ChangedRun> {
    let mut runs = Vec::new();
    let mut deletions = Vec::new();
    let mut additions = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        match line.kind {
            DiffLineKind::Deletion => deletions.push(index),
            DiffLineKind::Addition => additions.push(index),
            DiffLineKind::Context => push_run(&mut runs, &mut deletions, &mut additions),
            DiffLineKind::BothEofNoNewline
            | DiffLineKind::OldEofNoNewline
            | DiffLineKind::NewEofNoNewline => {}
        }
    }
    push_run(&mut runs, &mut deletions, &mut additions);
    runs
}

fn push_run(runs: &mut Vec<ChangedRun>, deletions: &mut Vec<usize>, additions: &mut Vec<usize>) {
    if deletions.is_empty() && additions.is_empty() {
        return;
    }
    runs.push(ChangedRun {
        deletions: mem::take(deletions),
        additions: mem::take(additions),
    });
}

/// Finds a maximum-weight, order-preserving line alignment.
///
/// Similarity is a stable integer score. The secondary pair-count component
/// and explicit direction preference make every tie deterministic.
fn pair_run(run: &ChangedRun, lines: &[DiffLine]) -> Vec<(usize, usize)> {
    let old_count = run.deletions.len();
    let new_count = run.additions.len();
    let width = new_count + 1;
    let mut scores = vec![0u64; (old_count + 1) * width];
    let mut directions = vec![0u8; scores.len()];
    let pair_scale = old_count.min(new_count) as u64 + 1;

    for old in 1..=old_count {
        for new in 1..=new_count {
            let position = old * width + new;
            let above = scores[(old - 1) * width + new];
            let left = scores[old * width + new - 1];
            let (mut best, mut direction) = if above >= left { (above, 1) } else { (left, 2) };

            let similarity = line_similarity(
                &lines[run.deletions[old - 1]].content,
                &lines[run.additions[new - 1]].content,
            );
            let eligible =
                similarity >= MIN_PAIR_SIMILARITY_PER_MILLE || (old_count == 1 && new_count == 1);
            if eligible {
                let paired = scores[(old - 1) * width + new - 1]
                    .saturating_add((similarity as u64).saturating_mul(pair_scale))
                    .saturating_add(1);
                if paired >= best {
                    best = paired;
                    direction = 3;
                }
            }
            scores[position] = best;
            directions[position] = direction;
        }
    }

    let mut pairs = Vec::new();
    let (mut old, mut new) = (old_count, new_count);
    while old > 0 && new > 0 {
        match directions[old * width + new] {
            3 => {
                pairs.push((run.deletions[old - 1], run.additions[new - 1]));
                old -= 1;
                new -= 1;
            }
            1 => old -= 1,
            2 => new -= 1,
            _ => unreachable!("an interior alignment cell always has a direction"),
        }
    }
    pairs.reverse();
    pairs
}

/// Scores shared outer bytes, excluding a common line ending so unrelated
/// lines do not become candidates merely because both end in `\n`.
fn line_similarity(old: &[u8], new: &[u8]) -> usize {
    let old = line_body(old);
    let new = line_body(new);
    if old == new {
        return 1_000;
    }
    if old.is_empty() || new.is_empty() {
        return 0;
    }

    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    let available = old.len().min(new.len()).saturating_sub(prefix);
    let suffix = old[old.len() - available..]
        .iter()
        .rev()
        .zip(new[new.len() - available..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    prefix.saturating_add(suffix).saturating_mul(2_000) / old.len().saturating_add(new.len())
}

fn line_body(mut line: &[u8]) -> &[u8] {
    if let Some(stripped) = line.strip_suffix(b"\n") {
        line = stripped;
    }
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Tokenizes raw bytes without assuming an encoding. ASCII words (plus any
/// adjacent high bytes), horizontal whitespace and line endings form tokens;
/// punctuation remains individually addressable.
fn tokenize(line: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut start = 0;
    while start < line.len() {
        let byte = line[start];
        let end = if byte == b'\r' && line.get(start + 1) == Some(&b'\n') {
            start + 2
        } else if matches!(byte, b'\r' | b'\n') {
            start + 1
        } else if is_word_byte(byte) {
            take_while(line, start, is_word_byte)
        } else if byte.is_ascii_whitespace() {
            take_while(line, start, |byte| {
                byte.is_ascii_whitespace() && !matches!(byte, b'\r' | b'\n')
            })
        } else {
            start + 1
        };
        tokens.push(Token { start, end });
        start = end;
    }
    tokens
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

fn take_while(line: &[u8], start: usize, predicate: impl Fn(u8) -> bool) -> usize {
    let mut end = start + 1;
    while end < line.len() && predicate(line[end]) {
        end += 1;
    }
    end
}

/// Uses an exact token LCS to turn a paired line into changed byte ranges.
fn changed_token_ranges(
    old: &[u8],
    old_tokens: &[Token],
    new: &[u8],
    new_tokens: &[Token],
) -> (Vec<IntraLineRange>, Vec<IntraLineRange>) {
    let width = new_tokens.len() + 1;
    let mut lengths = vec![0u16; (old_tokens.len() + 1) * width];
    for old_index in (0..old_tokens.len()).rev() {
        for new_index in (0..new_tokens.len()).rev() {
            let position = old_index * width + new_index;
            lengths[position] =
                if tokens_equal(old, old_tokens[old_index], new, new_tokens[new_index]) {
                    lengths[(old_index + 1) * width + new_index + 1].saturating_add(1)
                } else {
                    lengths[(old_index + 1) * width + new_index]
                        .max(lengths[old_index * width + new_index + 1])
                };
        }
    }

    let mut matches = Vec::new();
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old_tokens.len() && new_index < new_tokens.len() {
        if tokens_equal(old, old_tokens[old_index], new, new_tokens[new_index])
            && lengths[old_index * width + new_index]
                == lengths[(old_index + 1) * width + new_index + 1].saturating_add(1)
        {
            matches.push((old_index, new_index));
            old_index += 1;
            new_index += 1;
        } else if lengths[(old_index + 1) * width + new_index]
            >= lengths[old_index * width + new_index + 1]
        {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }

    let old_matches = matches.iter().map(|(old, _)| *old).collect::<Vec<_>>();
    let new_matches = matches.iter().map(|(_, new)| *new).collect::<Vec<_>>();
    (
        unmatched_ranges(old_tokens, &old_matches),
        unmatched_ranges(new_tokens, &new_matches),
    )
}

fn tokens_equal(old: &[u8], old_token: Token, new: &[u8], new_token: Token) -> bool {
    old[old_token.start..old_token.end] == new[new_token.start..new_token.end]
}

fn unmatched_ranges(tokens: &[Token], matched: &[usize]) -> Vec<IntraLineRange> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    for &matched_index in matched {
        push_unmatched_range(&mut ranges, tokens, cursor, matched_index);
        cursor = matched_index + 1;
    }
    push_unmatched_range(&mut ranges, tokens, cursor, tokens.len());
    ranges
}

fn push_unmatched_range(
    ranges: &mut Vec<IntraLineRange>,
    tokens: &[Token],
    start: usize,
    end: usize,
) {
    if start < end {
        ranges.push(IntraLineRange {
            start: tokens[start].start,
            end: tokens[end - 1].end,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_word_replacement_marks_the_whole_word_on_each_side() {
        let mut hunk = replacement_hunk(&[b"let color = red;\n"], &[b"let color = blue;\n"]);

        annotate(&mut hunk);

        assert_eq!(hunk.lines[0].paired_line_index, Some(1));
        assert_eq!(hunk.lines[1].paired_line_index, Some(0));
        assert_eq!(
            hunk.lines[0].intra_line_ranges,
            Some(vec![IntraLineRange { start: 12, end: 15 }])
        );
        assert_eq!(
            hunk.lines[1].intra_line_ranges,
            Some(vec![IntraLineRange { start: 12, end: 16 }])
        );
    }

    #[test]
    fn shifted_replacement_runs_pair_by_content_in_source_order() {
        let mut hunk = replacement_hunk(
            &[b"alpha old\n", b"beta same\n"],
            &[b"inserted\n", b"alpha new\n", b"beta changed\n"],
        );

        annotate(&mut hunk);

        assert_eq!(hunk.lines[0].paired_line_index, Some(3));
        assert_eq!(hunk.lines[1].paired_line_index, Some(4));
        assert_eq!(hunk.lines[2].paired_line_index, None);
        assert_eq!(hunk.lines[3].paired_line_index, Some(0));
        assert_eq!(hunk.lines[4].paired_line_index, Some(1));
    }

    #[test]
    fn separated_word_edits_produce_separate_byte_ranges() {
        let mut hunk = replacement_hunk(
            &[b"prefix old middle stale suffix\n"],
            &[b"prefix new middle fresh suffix\n"],
        );

        annotate(&mut hunk);

        assert_eq!(
            hunk.lines[0].intra_line_ranges,
            Some(vec![
                IntraLineRange { start: 7, end: 10 },
                IntraLineRange { start: 18, end: 23 },
            ])
        );
        assert_eq!(
            hunk.lines[0].intra_line_ranges,
            hunk.lines[1].intra_line_ranges
        );
    }

    #[test]
    fn a_one_sided_insertion_keeps_an_explicit_empty_range_set() {
        let mut hunk = replacement_hunk(&[b"name value\n"], &[b"name extra value\n"]);

        annotate(&mut hunk);

        assert_eq!(hunk.lines[0].intra_line_ranges, Some(Vec::new()));
        assert_eq!(
            hunk.lines[1].intra_line_ranges,
            Some(vec![IntraLineRange { start: 5, end: 11 }])
        );
    }

    #[test]
    fn a_complete_rewrite_is_one_pair_with_full_content_ranges() {
        let mut hunk = replacement_hunk(&[b"abc\n"], &[b"xyz\n"]);

        annotate(&mut hunk);

        assert_eq!(
            hunk.lines[0].intra_line_ranges,
            Some(vec![IntraLineRange { start: 0, end: 4 }])
        );
        assert_eq!(
            hunk.lines[1].intra_line_ranges,
            Some(vec![IntraLineRange { start: 0, end: 4 }])
        );
    }

    #[test]
    fn an_overlong_pair_degrades_the_whole_hunk_without_partial_metadata() {
        let old = vec![b'a'; MAX_INTRA_LINE_BYTES + 1];
        let new = vec![b'b'; MAX_INTRA_LINE_BYTES + 1];
        let mut hunk = replacement_hunk(&[old.as_slice()], &[new.as_slice()]);

        annotate(&mut hunk);

        assert_eq!(
            hunk.intra_line_degradation,
            Some(IntraLineDegradation::LineTooLong {
                limit: MAX_INTRA_LINE_BYTES
            })
        );
        assert_plain(&hunk);
    }

    #[test]
    fn a_pathological_run_names_the_pairing_bound() {
        let old = vec![b"a\n".as_slice(); 400];
        let new = vec![b"b\n".as_slice(); 400];
        let mut hunk = replacement_hunk(&old, &new);

        annotate(&mut hunk);

        assert_eq!(
            hunk.intra_line_degradation,
            Some(IntraLineDegradation::PairingTooLarge {
                limit: MAX_INTRA_LINE_COMPARISONS
            })
        );
        assert_plain(&hunk);
    }

    fn replacement_hunk(old: &[&[u8]], new: &[&[u8]]) -> Hunk {
        let mut lines = old
            .iter()
            .map(|content| line(DiffLineKind::Deletion, content))
            .collect::<Vec<_>>();
        lines.extend(
            new.iter()
                .map(|content| line(DiffLineKind::Addition, content)),
        );
        Hunk {
            old_start: 1,
            old_lines: old.len() as u32,
            new_start: 1,
            new_lines: new.len() as u32,
            header: Vec::new(),
            intra_line_degradation: None,
            lines,
        }
    }

    fn line(kind: DiffLineKind, content: &[u8]) -> DiffLine {
        DiffLine {
            kind,
            old_line_number: None,
            new_line_number: None,
            content: content.to_vec(),
            paired_line_index: None,
            intra_line_ranges: None,
        }
    }

    fn assert_plain(hunk: &Hunk) {
        assert!(
            hunk.lines
                .iter()
                .all(|line| line.paired_line_index.is_none() && line.intra_line_ranges.is_none())
        );
    }
}
