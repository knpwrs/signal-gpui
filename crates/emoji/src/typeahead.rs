//! Composer helpers: `:shortcode` type-ahead and jumbomoji detection.
//!
//! Mirrors Signal Desktop's behavior: typing `:` followed by at least two
//! shortcode characters opens the completion popup, and typing a complete
//! `:shortcode:` replaces it with the emoji.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::{Emoji, by_glyph, by_shortcode};

/// Minimum query length after the colon before suggestions appear.
pub const MIN_QUERY_LEN: usize = 2;

/// Jumbomoji: messages of at most this many emoji (and nothing else) render large.
pub const MAX_JUMBO: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Byte range to replace, from the `:` up to the cursor.
    pub range: Range<usize>,
    /// The text typed after the colon.
    pub query: String,
}

fn is_shortcode_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// Whether a `:` preceded by `before` can start a shortcode. Rules out times
/// (`10:30`), URLs (`https://`) and words (`re:`).
fn can_open(before: Option<char>) -> bool {
    match before {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\'' | '“' | '‘'),
    }
}

/// Finds the shortcode run ending at `end` and the `:` before it.
/// Returns the byte offset of the colon.
fn colon_before(text: &str, end: usize) -> Option<usize> {
    let head = text.get(..end)?;
    let run_start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_shortcode_char(*c))
        .last()
        .map_or(end, |(i, _)| i);
    let colon = run_start.checked_sub(1)?;
    if head.as_bytes()[colon] != b':' {
        return None;
    }
    let before = head[..colon].chars().next_back();
    can_open(before).then_some(colon)
}

/// If the cursor is at the end of a `:query`, returns the span and query.
///
/// `cursor` is a byte offset into `text`.
pub fn completion_at(text: &str, cursor: usize) -> Option<Completion> {
    let colon = colon_before(text, cursor)?;
    let query = &text[colon + 1..cursor];
    if query.chars().count() < MIN_QUERY_LEN || query.starts_with(['-', '_']) {
        return None;
    }
    Some(Completion {
        range: colon..cursor,
        query: query.to_owned(),
    })
}

/// If the text just before `cursor` is a complete `:shortcode:`, returns the
/// byte range to replace and the emoji.
pub fn shortcode_replacement_at(text: &str, cursor: usize) -> Option<(Range<usize>, &'static Emoji)> {
    let head = text.get(..cursor)?;
    let closing = head.len().checked_sub(1)?;
    if head.as_bytes()[closing] != b':' {
        return None;
    }
    let colon = colon_before(text, closing)?;
    let emoji = by_shortcode(&text[colon + 1..closing])?;
    Some((colon..cursor, emoji))
}

/// Number of emoji if `text` consists only of 1..=[`MAX_JUMBO`] emoji
/// (whitespace allowed), else `None`.
pub fn jumbo_count(text: &str) -> Option<usize> {
    let mut count = 0;
    for grapheme in text.graphemes(true) {
        if grapheme.chars().all(char::is_whitespace) {
            continue;
        }
        by_glyph(grapheme)?;
        count += 1;
        if count > MAX_JUMBO {
            return None;
        }
    }
    (count > 0).then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(text: &str) -> Option<Completion> {
        completion_at(text, text.len())
    }

    #[test]
    fn opens_after_two_chars() {
        assert_eq!(at_end(":s"), None);
        assert_eq!(
            at_end(":sm"),
            Some(Completion {
                range: 0..3,
                query: "sm".into()
            })
        );
        assert_eq!(at_end("hello :joy").unwrap().query, "joy");
        assert_eq!(at_end("(:+1").unwrap().query, "+1");
    }

    #[test]
    fn ignores_non_shortcode_colons() {
        assert_eq!(at_end("at 10:30"), None);
        assert_eq!(at_end("https://example"), None);
        assert_eq!(at_end("re:subject"), None);
        assert_eq!(at_end(":-)"), None);
        assert_eq!(at_end("hello :joy "), None);
    }

    #[test]
    fn respects_cursor_position() {
        let text = ":smile and more";
        assert_eq!(completion_at(text, 6).unwrap().query, "smile");
        assert_eq!(completion_at(text, 10), None);
    }

    #[test]
    fn multibyte_text_before_colon() {
        let text = "héllo 😀 :ca";
        let c = at_end(text).unwrap();
        assert_eq!(&text[c.range.clone()], ":ca");
    }

    #[test]
    fn replaces_complete_shortcodes() {
        let text = "that is :joy:";
        let (range, emoji) = shortcode_replacement_at(text, text.len()).unwrap();
        assert_eq!(&text[range], ":joy:");
        assert_eq!(emoji.glyph, "😂");

        assert!(shortcode_replacement_at("10:30:", 6).is_none());
        assert!(shortcode_replacement_at(":nope_not_real:", 15).is_none());
        assert!(shortcode_replacement_at(":joy", 4).is_none());
    }

    #[test]
    fn jumbomoji() {
        assert_eq!(jumbo_count("😂"), Some(1));
        assert_eq!(jumbo_count(" 😂 👍🏽 🇺🇸 "), Some(3));
        assert_eq!(jumbo_count("❤"), Some(1));
        assert_eq!(jumbo_count("👩🏽‍💻"), Some(1));
        assert_eq!(jumbo_count("😂😂😂😂😂😂"), None);
        assert_eq!(jumbo_count("lol 😂"), None);
        assert_eq!(jumbo_count(""), None);
        assert_eq!(jumbo_count("   "), None);
    }
}
