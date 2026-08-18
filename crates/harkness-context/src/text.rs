//! Bounding caller and repository text, once.

/// The largest index at or below `max_bytes` that splits `text` between
/// characters.
///
/// Every bound in this crate is a byte count and every value it bounds is UTF-8,
/// so each one owes the same walk backwards off a multi-byte character. Written
/// here once because the two callers disagree about what to do afterwards —
/// `provenance` truncates silently, an inventory diagnostic marks the cut — and
/// that disagreement is a policy rather than a second algorithm.
pub(crate) fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return text.len();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::floor_char_boundary;

    #[test]
    fn a_bound_inside_a_character_walks_back_to_its_start() {
        // "aéb" is four bytes: 'a', then "é" across 1..3, then 'b'. A bound of
        // 2 lands inside the two-byte character and walks back to 1; a bound of
        // 3 is already the start of 'b'.
        assert_eq!(floor_char_boundary("aéb", 2), 1);
        assert_eq!(floor_char_boundary("aéb", 3), 3);
        assert_eq!(floor_char_boundary("abc", 2), 2);
    }

    #[test]
    fn text_within_the_bound_keeps_all_of_its_bytes() {
        assert_eq!(floor_char_boundary("abc", 8), 3);
        assert_eq!(floor_char_boundary("", 8), 0);
    }

    #[test]
    fn a_zero_bound_is_reachable_rather_than_a_panic() {
        assert_eq!(floor_char_boundary("étoile", 1), 0);
    }
}
