//! The formatting of the rich input's text: one [`StyleSet`] per byte of
//! the text, kept in step with every edit, and turned into Signal's
//! [`StyleSpan`]s when the text is sent.
//!
//! A set per byte, rather than a list of spans to shift, makes every edit a
//! splice of two vectors the same length: spans that an edit cuts, joins or
//! empties come out right by construction. Messages are capped at 64 KiB,
//! so it is at most 64 KiB of sets.
//!
//! Newlines never carry a style, as in Quill, which formats the text in a
//! line and not the line break (Desktop's `ts/quill/formatting/menu.dom.tsx`
//! special-cases newline-only ops for the same reason).

use std::ops::Range;

use signal_store::{StyleSpan, TextStyle};

/// Every style, in the order spans over the same text are listed.
pub const ALL: [TextStyle; 5] =
    [TextStyle::Bold, TextStyle::Italic, TextStyle::Strikethrough, TextStyle::Monospace, TextStyle::Spoiler];

/// The styles on one byte.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct StyleSet(u8);

fn bit(style: TextStyle) -> u8 {
    match style {
        TextStyle::Bold => 1,
        TextStyle::Italic => 2,
        TextStyle::Strikethrough => 4,
        TextStyle::Monospace => 8,
        TextStyle::Spoiler => 16,
    }
}

impl StyleSet {
    pub const NONE: StyleSet = StyleSet(0);

    pub fn has(self, style: TextStyle) -> bool {
        self.0 & bit(style) != 0
    }

    pub fn with(self, style: TextStyle, on: bool) -> StyleSet {
        if on { StyleSet(self.0 | bit(style)) } else { StyleSet(self.0 & !bit(style)) }
    }
}

/// The text's formatting: a set for each byte.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Styles(Vec<StyleSet>);

impl Styles {
    /// No formatting over `len` bytes.
    pub fn plain(len: usize) -> Styles {
        Styles(vec![StyleSet::NONE; len])
    }

    /// `spans` over `text`; spans past its end or inside a character are
    /// clipped to what fits.
    pub fn from_spans(text: &str, spans: &[StyleSpan]) -> Styles {
        let mut styles = Styles::plain(text.len());
        for span in spans {
            let end = span.start.saturating_add(span.len).min(text.len());
            if span.start < end {
                styles.set(text, span.start..end, span.style, true);
            }
        }
        styles
    }

    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The styles on the byte at `at` (none past the end).
    pub fn at(&self, at: usize) -> StyleSet {
        self.0.get(at).copied().unwrap_or_default()
    }

    /// Signal's spans for `text`: each style's longest runs, ordered by
    /// start, then length, then style.
    pub fn to_spans(&self, text: &str) -> Vec<StyleSpan> {
        debug_assert_eq!(text.len(), self.0.len());
        let mut spans = Vec::new();
        for style in ALL {
            let mut start = None;
            for (at, set) in self.0.iter().chain(std::iter::once(&StyleSet::NONE)).enumerate() {
                match (set.has(style), start) {
                    (true, None) => start = Some(at),
                    (false, Some(from)) => {
                        spans.push(StyleSpan { start: from, len: at - from, style });
                        start = None;
                    }
                    _ => {}
                }
            }
        }
        spans.sort_by_key(|s| (s.start, s.len, ALL.iter().position(|a| *a == s.style)));
        spans
    }

    /// Replaces the sets of `range` (bytes of the text before the edit) with
    /// `inserted`'s, each `style` (newlines none), as the text's own
    /// `replace_range(range, inserted)` does.
    pub fn splice(&mut self, range: Range<usize>, inserted: &str, style: StyleSet) {
        let sets = inserted.bytes().map(|b| if b == b'\n' { StyleSet::NONE } else { style });
        self.0.splice(range, sets);
    }

    /// Replaces the sets of `range` with `with`'s (the styles of the text
    /// that replaces it).
    pub fn replace(&mut self, range: Range<usize>, with: Styles) {
        self.0.splice(range, with.0);
    }

    /// The sets of `range`, for the text in it.
    pub fn slice(&self, range: Range<usize>) -> Styles {
        Styles(self.0[range].to_vec())
    }

    /// Turns `style` on or off over `range` of `text` (newlines stay bare).
    pub fn set(&mut self, text: &str, range: Range<usize>, style: TextStyle, on: bool) {
        let bytes = text.as_bytes();
        for at in range {
            if bytes.get(at) != Some(&b'\n')
                && let Some(set) = self.0.get_mut(at)
            {
                *set = set.with(style, on);
            }
        }
    }

    /// Whether every character of `range` but its newlines has `style`, as
    /// Quill's `getFormat` over a selection reports a format all of it
    /// shares. A range of nothing but newlines has none.
    pub fn all_have(&self, text: &str, range: Range<usize>, style: TextStyle) -> bool {
        let bytes = text.as_bytes();
        let mut any = false;
        for at in range {
            if bytes.get(at) == Some(&b'\n') {
                continue;
            }
            if !self.at(at).has(style) {
                return false;
            }
            any = true;
        }
        any
    }

    /// The styles text typed at `range` takes, as in Quill (where the
    /// browser types into the text node at the caret): over a selection,
    /// its first character's; at a caret, the character before it on the
    /// same line, or at a line's start the one after it.
    pub fn inherited(&self, text: &str, range: Range<usize>) -> StyleSet {
        let bytes = text.as_bytes();
        if range.start < range.end {
            return self.at(range.start);
        }
        let at = range.start.min(text.len());
        if at > 0 && bytes[at - 1] != b'\n' {
            // The character before: its last byte has its styles.
            return self.at(at - 1);
        }
        if at < bytes.len() && bytes[at] != b'\n' {
            return self.at(at);
        }
        StyleSet::NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TextStyle::*;

    /// Applies an edit to both the text and its styles, as the input does.
    fn edit(text: &mut String, styles: &mut Styles, range: Range<usize>, inserted: &str) {
        let style = styles.inherited(text, range.clone());
        styles.splice(range.clone(), inserted, style);
        text.replace_range(range, inserted);
        assert_eq!(styles.len(), text.len());
    }

    /// `(styled text, style)` for each span, for readable assertions.
    fn spans(text: &str, styles: &Styles) -> Vec<(String, TextStyle)> {
        styles.to_spans(text).iter().map(|s| (text[s.start..s.start + s.len].to_owned(), s.style)).collect()
    }

    fn bold(text: &str, range: Range<usize>) -> Styles {
        let mut styles = Styles::plain(text.len());
        styles.set(text, range, Bold, true);
        styles
    }

    #[test]
    fn spans_round_trip_and_merge() {
        let text = "say hello there";
        let input = [
            StyleSpan { start: 4, len: 5, style: Bold },
            StyleSpan { start: 6, len: 9, style: Italic },
            StyleSpan { start: 7, len: 2, style: Bold },
        ];
        let styles = Styles::from_spans(text, &input);
        assert_eq!(styles.to_spans(text), [
            StyleSpan { start: 4, len: 5, style: Bold },
            StyleSpan { start: 6, len: 9, style: Italic },
        ]);
        // Past the end: clipped.
        let styles = Styles::from_spans("hi", &[StyleSpan { start: 1, len: 9, style: Spoiler }]);
        assert_eq!(styles.to_spans("hi"), [StyleSpan { start: 1, len: 1, style: Spoiler }]);
    }

    #[test]
    fn typing_inside_or_at_the_end_continues_the_style() {
        let mut text = "a bold b".to_owned();
        let mut styles = bold(&text, 2..6);
        edit(&mut text, &mut styles, 4..4, "XX");
        assert_eq!(spans(&text, &styles), [("boXXld".into(), Bold)]);
        // At its end: Quill takes the character before.
        edit(&mut text, &mut styles, 8..8, "!");
        assert_eq!(spans(&text, &styles), [("boXXld!".into(), Bold)]);
        // Just before it, after a plain space: plain.
        edit(&mut text, &mut styles, 2..2, "_");
        assert_eq!(spans(&text, &styles), [("boXXld!".into(), Bold)]);
        assert_eq!(text, "a _boXXld! b");
    }

    #[test]
    fn typing_at_a_line_start_takes_the_next_character() {
        let mut text = "x\nbold".to_owned();
        let mut styles = bold(&text, 2..6);
        edit(&mut text, &mut styles, 2..2, "A");
        assert_eq!(spans(&text, &styles), [("Abold".into(), Bold)]);
        let mut text = "bold".to_owned();
        let mut styles = bold(&text, 0..4);
        edit(&mut text, &mut styles, 0..0, "A");
        assert_eq!(spans(&text, &styles), [("Abold".into(), Bold)]);
    }

    #[test]
    fn deleting_across_a_boundary_keeps_what_is_left() {
        let mut text = "plain bold plain".to_owned();
        let mut styles = bold(&text, 6..10);
        // From inside the plain text into the bold.
        edit(&mut text, &mut styles, 3..8, "");
        assert_eq!(text, "plald plain");
        assert_eq!(spans(&text, &styles), [("ld".into(), Bold)]);
        // The whole span: gone.
        edit(&mut text, &mut styles, 3..5, "");
        assert_eq!(spans(&text, &styles), []);
    }

    #[test]
    fn deleting_between_two_spans_joins_them() {
        let mut text = "aa bb".to_owned();
        let mut styles = bold(&text, 0..2);
        styles.set(&text, 3..5, Bold, true);
        edit(&mut text, &mut styles, 2..3, "");
        assert_eq!(spans(&text, &styles), [("aabb".into(), Bold)]);
    }

    #[test]
    fn typing_over_a_selection_takes_its_first_character_style() {
        let mut text = "one two".to_owned();
        let mut styles = bold(&text, 4..7);
        edit(&mut text, &mut styles, 4..7, "2");
        assert_eq!(spans(&text, &styles), [("2".into(), Bold)]);
        // Starting in plain text: plain.
        let mut text = "one two".to_owned();
        let mut styles = bold(&text, 4..7);
        edit(&mut text, &mut styles, 2..6, "-");
        assert_eq!(text, "on-o");
        assert_eq!(spans(&text, &styles), [("o".into(), Bold)]);
    }

    #[test]
    fn pasted_or_inserted_text_splits_a_span_when_plain() {
        let mut text = "boldtext".to_owned();
        let mut styles = bold(&text, 0..8);
        styles.splice(4..4, " pasted ", StyleSet::NONE);
        text.replace_range(4..4, " pasted ");
        assert_eq!(spans(&text, &styles), [("bold".into(), Bold), ("text".into(), Bold)]);
    }

    #[test]
    fn multibyte_text_and_newlines() {
        let mut text = "é😀".to_owned();
        let mut styles = Styles::plain(text.len());
        styles.set(&text.clone(), 0..text.len(), Italic, true);
        let end = text.len();
        edit(&mut text, &mut styles, end..end, "\n");
        // The newline stays bare, and the new line starts plain (nothing
        // after the caret on its line), so ñ typed there isn't italic.
        let end = text.len();
        edit(&mut text, &mut styles, end..end, "ñ");
        assert_eq!(spans(&text, &styles), [("é😀".into(), Italic)]);
        // A style set across lines skips the newline: two spans.
        let text = "ab\ncd";
        let mut styles = Styles::plain(text.len());
        styles.set(text, 0..5, Strikethrough, true);
        assert_eq!(spans(text, &styles), [("ab".into(), Strikethrough), ("cd".into(), Strikethrough)]);
        assert!(styles.all_have(text, 0..5, Strikethrough), "newlines don't count");
        assert!(!styles.all_have(text, 2..3, Strikethrough), "a newline alone has none");
    }

    #[test]
    fn toggling_reports_what_all_of_the_selection_has() {
        let text = "mixed bold";
        let styles = bold(text, 6..10);
        assert!(styles.all_have(text, 6..10, Bold));
        assert!(!styles.all_have(text, 4..10, Bold));
        assert!(!styles.all_have(text, 6..10, Italic));
    }
}
