//! Formatting in the composer, written as markers around the text:
//! `*bold*`, `_italic_`, `~strikethrough~`, `` `monospace` `` and
//! `||spoiler||` (`**bold**` and `~~strikethrough~~` too). The input can't
//! show styled text, so markers stand in for Desktop's formatting menu and
//! its shortcuts wrap the selection in them. On sending they become Signal's
//! styles, which every client shows.
//!
//! As in WhatsApp, a marker only counts at a word's edge: an opening one
//! after a space, punctuation or the start, before a non-space; a closing
//! one after a non-space, before a space, punctuation or the end, on the
//! same line. So `snake_case_name` and `2*3*4` stay as typed.

use std::ops::Range;

use signal_store::{StyleSpan, TextStyle};

/// Longest first, so `**` isn't read as two `*`.
const MARKERS: [(&str, TextStyle); 7] = [
    ("||", TextStyle::Spoiler),
    ("**", TextStyle::Bold),
    ("~~", TextStyle::Strikethrough),
    ("*", TextStyle::Bold),
    ("_", TextStyle::Italic),
    ("~", TextStyle::Strikethrough),
    ("`", TextStyle::Monospace),
];

/// The marker each style is written with.
pub fn marker(style: TextStyle) -> &'static str {
    match style {
        TextStyle::Bold => "*",
        TextStyle::Italic => "_",
        TextStyle::Strikethrough => "~",
        TextStyle::Monospace => "`",
        TextStyle::Spoiler => "||",
    }
}

/// The text without its markers, and the styles they marked (spans over
/// that text).
pub fn parse(text: &str) -> (String, Vec<StyleSpan>) {
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::new();
    parse_into(text, &mut out, &mut spans);
    spans.sort_by_key(|s| (s.start, s.len, rank(s.style)));
    (out, spans)
}

/// A fixed order for styles over the same text.
fn rank(style: TextStyle) -> u8 {
    match style {
        TextStyle::Bold => 0,
        TextStyle::Italic => 1,
        TextStyle::Strikethrough => 2,
        TextStyle::Monospace => 3,
        TextStyle::Spoiler => 4,
    }
}

fn parse_into(text: &str, out: &mut String, spans: &mut Vec<StyleSpan>) {
    let mut i = 0;
    while i < text.len() {
        if let Some((open, style)) = MARKERS.iter().find(|(m, _)| text[i..].starts_with(m)).copied()
            && opens(text, i, open)
            && let Some(close) = closing(text, i + open.len(), open)
        {
            let inner = &text[i + open.len()..close];
            let start = out.len();
            if style == TextStyle::Monospace {
                // Code is shown as written.
                out.push_str(inner);
            } else {
                parse_into(inner, out, spans);
            }
            spans.push(StyleSpan { start, len: out.len() - start, style });
            i = close + open.len();
            continue;
        }
        let c = text[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
}

/// A word's edge: a space or punctuation (not a letter or digit).
fn edge(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_alphanumeric())
}

fn opens(text: &str, at: usize, marker: &str) -> bool {
    let before = text[..at].chars().next_back();
    let after = text[at + marker.len()..].chars().next();
    edge(before) && after.is_some_and(|c| !c.is_whitespace())
}

/// Where `marker` closes a run that starts at `from`, on the same line.
fn closing(text: &str, from: usize, marker: &str) -> Option<usize> {
    let line_end = text[from..].find('\n').map_or(text.len(), |n| from + n);
    let mut search = from;
    while let Some(found) = text[search..line_end].find(marker) {
        let at = search + found;
        let before = text[..at].chars().next_back();
        let after = text[at + marker.len()..].chars().next();
        let long_enough = at > from;
        if long_enough && before.is_some_and(|c| !c.is_whitespace()) && edge(after) {
            // `*` isn't the start of a `**` closing something longer.
            if !(marker.len() == 1 && after == marker.chars().next()) {
                return Some(at);
            }
        }
        search = at + marker.len();
    }
    None
}

/// `body` with its styles written back as markers (the reverse of
/// [`parse`]), for editing a sent message. Styles that cross each other are
/// written as nested as they can be.
pub fn to_markers(body: &str, styles: &[StyleSpan]) -> String {
    let mut styles: Vec<&StyleSpan> = styles
        .iter()
        .filter(|s| s.len > 0 && s.start + s.len <= body.len() && body.is_char_boundary(s.start) && body.is_char_boundary(s.start + s.len))
        .collect();
    // Outer before inner where they start together.
    styles.sort_by_key(|s| (s.start, std::cmp::Reverse(s.len)));
    let mut out = String::with_capacity(body.len() + styles.len() * 4);
    let mut open: Vec<&StyleSpan> = Vec::new();
    let mut next = styles.into_iter().peekable();
    for (at, c) in body.char_indices().chain(std::iter::once((body.len(), '\0'))) {
        // Close what ends here, innermost first.
        while let Some(last) = open.last()
            && last.start + last.len <= at
        {
            out.push_str(marker(last.style));
            open.pop();
        }
        while let Some(span) = next.next_if(|s| s.start == at) {
            out.push_str(marker(span.style));
            open.push(span);
        }
        if at < body.len() {
            out.push(c);
        }
    }
    // Anything left open (crossing styles) closes at the end.
    while let Some(span) = open.pop() {
        out.push_str(marker(span.style));
    }
    out
}

/// Desktop's formatting shortcuts, with markers: wraps the selection in
/// `style`'s marker, or unwraps it if it's already wrapped. An empty
/// selection gets a pair to type between. Returns the new text and what to
/// select (the same words, inside the markers).
pub fn toggle(text: &str, selection: Range<usize>, style: TextStyle) -> (String, Range<usize>) {
    let m = marker(style);
    let (a, b) = (selection.start.min(text.len()), selection.end.min(text.len()));
    let (start, end) = (a.min(b), a.max(b));
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return (text.to_owned(), start..end);
    }
    let wrapped = start >= m.len()
        && text.get(start - m.len()..start) == Some(m)
        && text.get(end..end + m.len()) == Some(m);
    if wrapped {
        let text = format!("{}{}{}", &text[..start - m.len()], &text[start..end], &text[end + m.len()..]);
        return (text, start - m.len()..end - m.len());
    }
    // The selected words, without the spaces around them (a marker before a
    // space wouldn't open).
    let selected = &text[start..end];
    let trimmed_start = start + (selected.len() - selected.trim_start().len());
    let trimmed_end = (end - (selected.len() - selected.trim_end().len())).max(trimmed_start);
    let text = format!(
        "{}{m}{}{m}{}",
        &text[..trimmed_start],
        &text[trimmed_start..trimmed_end],
        &text[trimmed_end..]
    );
    (text, trimmed_start + m.len()..trimmed_end + m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(text: &str) -> (String, Vec<(String, TextStyle)>) {
        let (plain, spans) = parse(text);
        let parts = spans.iter().map(|s| (plain[s.start..s.start + s.len].to_owned(), s.style)).collect();
        (plain, parts)
    }

    #[test]
    fn markers_become_styles() {
        assert_eq!(styled("a *bold* move"), ("a bold move".into(), vec![("bold".into(), TextStyle::Bold)]));
        assert_eq!(styled("_so_ ~old~ `x = 1` ||ending||").1, vec![
            ("so".into(), TextStyle::Italic),
            ("old".into(), TextStyle::Strikethrough),
            ("x = 1".into(), TextStyle::Monospace),
            ("ending".into(), TextStyle::Spoiler),
        ]);
        assert_eq!(styled("**bold** and ~~gone~~").1, vec![
            ("bold".into(), TextStyle::Bold),
            ("gone".into(), TextStyle::Strikethrough),
        ]);
    }

    #[test]
    fn markers_nest_except_in_code() {
        assert_eq!(styled("*_both_*"), ("both".into(), vec![("both".into(), TextStyle::Bold), ("both".into(), TextStyle::Italic)]));
        assert_eq!(styled("`*not bold*`"), ("*not bold*".into(), vec![("*not bold*".into(), TextStyle::Monospace)]));
    }

    #[test]
    fn markers_inside_words_or_alone_stay_text() {
        for text in ["snake_case_name", "2*3*4", "a * b * c", "* not *", "price_", "**", "_\nx_", "mid*word* here"] {
            assert_eq!(styled(text), (text.to_owned(), vec![]), "{text:?}");
        }
        // Punctuation is a word's edge.
        assert_eq!(styled("(*yes*)").1, vec![("yes".into(), TextStyle::Bold)]);
    }

    #[test]
    fn styles_round_trip_through_markers() {
        for text in ["a *bold* _and_ ||hidden|| `code` ~gone~", "*_both_* end", "plain"] {
            let (plain, spans) = parse(text);
            assert_eq!(parse(&to_markers(&plain, &spans)), (plain.clone(), spans), "{text:?}");
        }
        // Received formatting written out for editing.
        let spans = [StyleSpan { start: 0, len: 5, style: TextStyle::Bold }, StyleSpan { start: 6, len: 5, style: TextStyle::Spoiler }];
        assert_eq!(to_markers("hello world", &spans), "*hello* ||world||");
    }

    #[test]
    fn shortcuts_wrap_and_unwrap_the_selection() {
        let (text, selection) = toggle("say hello there", 4..9, TextStyle::Bold);
        assert_eq!((text.as_str(), selection.clone()), ("say *hello* there", 5..10));
        let (text, selection) = toggle(&text, selection, TextStyle::Bold);
        assert_eq!((text.as_str(), selection), ("say hello there", 4..9));
        // Spaces at the selection's edges stay outside.
        assert_eq!(toggle("say hello there", 3..10, TextStyle::Spoiler).0, "say ||hello|| there");
        // Nothing selected: a pair to type between.
        assert_eq!(toggle("hi ", 3..3, TextStyle::Italic), ("hi __".into(), 4..4));
        // Reversed, or inside a character: no panic.
        assert_eq!(toggle("say hello", Range { start: 9, end: 4 }, TextStyle::Bold).0, "say *hello*");
        assert_eq!(toggle("é", 1..2, TextStyle::Bold).0, "é");
    }
}
