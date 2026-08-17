//! Bounded text handling shared by error details and `Debug` previews.
//!
//! Two rules live here, and both exist because everything in this crate either
//! came from a model endpoint or is on its way to one. A stored detail is
//! clamped so a provider's error body cannot become an unbounded string in a
//! record, and a `Debug` rendering is previewed so `{:?}` on a request cannot
//! dump a prompt into a log before redaction ([#103]) applies.
//!
//! Both cut on a character boundary rather than a byte index, so a preview of a
//! multi-byte character is never half of one.
//!
//! [#103]: https://github.com/fullstacktaiye/harkness/issues/103

use std::fmt;

/// Largest byte index at or below `limit` that starts a character.
///
/// `str::floor_char_boundary` is unstable, and slicing on a byte index that
/// lands inside a multi-byte character panics, so the boundary is found here
/// rather than assumed.
pub(crate) fn floor_char_boundary(text: &str, limit: usize) -> usize {
    if limit >= text.len() {
        return text.len();
    }
    let mut index = limit;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Clamps `text` to `limit` bytes, naming what was dropped.
///
/// The marker is part of the value rather than a rendering choice: a clamped
/// detail that reads as a complete sentence is worse than no detail, because
/// nothing downstream can tell it was cut.
pub(crate) fn clamp(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let boundary = floor_char_boundary(&text, limit);
    let dropped = text.len() - boundary;
    let mut clamped = text;
    clamped.truncate(boundary);
    clamped.push_str(&format!("… (+{dropped} bytes)"));
    clamped
}

/// A `Debug` rendering of borrowed text that shows a bounded prefix.
///
/// Rendered as a quoted prefix followed by the number of bytes it stands in
/// for, so a reader can tell a short value from a truncated one without having
/// to know the bound.
pub(crate) struct Preview<'a> {
    text: &'a str,
    limit: usize,
}

impl<'a> Preview<'a> {
    pub(crate) const fn new(text: &'a str, limit: usize) -> Self {
        Self { text, limit }
    }
}

impl fmt::Debug for Preview<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.text.len() <= self.limit {
            return fmt::Debug::fmt(self.text, formatter);
        }
        let boundary = floor_char_boundary(self.text, self.limit);
        let dropped = self.text.len() - boundary;
        fmt::Debug::fmt(&self.text[..boundary], formatter)?;
        write!(formatter, "… (+{dropped} bytes)")
    }
}

/// A `Debug` rendering of a slice that shows a bounded number of entries.
pub(crate) struct PreviewList<'a, T> {
    items: &'a [T],
    limit: usize,
}

impl<'a, T> PreviewList<'a, T> {
    pub(crate) const fn new(items: &'a [T], limit: usize) -> Self {
        Self { items, limit }
    }
}

impl<T: fmt::Debug> fmt::Debug for PreviewList<'_, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = self.items.len().min(self.limit);
        let mut list = formatter.debug_list();
        list.entries(&self.items[..shown]);
        if shown < self.items.len() {
            list.entry(&Elided(self.items.len() - shown));
        }
        list.finish()
    }
}

/// The tail of a [`PreviewList`], rendered without quotes.
struct Elided(usize);

impl fmt::Debug for Elided {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "… (+{} more)", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Preview, PreviewList, clamp, floor_char_boundary};

    #[test]
    fn a_boundary_never_lands_inside_a_character() {
        let text = "aé☕";
        assert_eq!(floor_char_boundary(text, 0), 0);
        assert_eq!(floor_char_boundary(text, 1), 1);
        assert_eq!(floor_char_boundary(text, 2), 1, "inside the two-byte é");
        assert_eq!(floor_char_boundary(text, 3), 3);
        assert_eq!(floor_char_boundary(text, 5), 3, "inside the three-byte ☕");
        assert_eq!(floor_char_boundary(text, 999), text.len());
    }

    #[test]
    fn clamping_names_what_it_dropped() {
        assert_eq!(clamp("short".to_owned(), 16), "short");
        assert_eq!(clamp("abcdefgh".to_owned(), 4), "abcd… (+4 bytes)");
        assert_eq!(
            clamp("aé☕".to_owned(), 2),
            "a… (+5 bytes)",
            "a clamp inside é retreats to the boundary and counts the real bytes"
        );
    }

    #[test]
    fn a_preview_shows_a_prefix_and_the_size_it_stands_for() {
        assert_eq!(format!("{:?}", Preview::new("short", 16)), "\"short\"");
        assert_eq!(
            format!("{:?}", Preview::new("abcdefgh", 4)),
            "\"abcd\"… (+4 bytes)"
        );
    }

    #[test]
    fn a_preview_list_names_the_entries_it_did_not_show() {
        assert_eq!(format!("{:?}", PreviewList::new(&[1, 2], 4)), "[1, 2]");
        assert_eq!(
            format!("{:?}", PreviewList::new(&[1, 2, 3, 4, 5], 2)),
            "[1, 2, … (+3 more)]"
        );
    }
}
