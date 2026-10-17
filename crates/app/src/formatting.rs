//! Formatting written as markers around the text: `*bold*`, `_italic_`,
//! `~strikethrough~`, `` `monospace` `` and `||spoiler||` (`**bold**` and
//! `~~strikethrough~~` too), for the plain text fields that can't show
//! styled text: a story's caption and a notification's inline reply. On
//! sending they become Signal's styles, which every client shows. (The
//! composer shows formatting as styled text instead, as Desktop's does;
//! see `crate::rich_input`.)
//!
//! As in WhatsApp, a marker only counts at a word's edge: an opening one
//! after a space, punctuation or the start, before a non-space; a closing
//! one after a non-space, before a space, punctuation or the end, on the
//! same line. So `snake_case_name` and `2*3*4` stay as typed.

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
}
