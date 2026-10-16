//! Spell checking the composer, as Desktop's (`app/spell_check.main.ts`,
//! Chromium's checker): misspelled words are underlined, and right-clicking
//! one offers its suggestions. Here macOS's `NSSpellChecker` does the
//! checking, off the main thread once typing pauses (it may be used from
//! any thread). Words in mentions and links aren't checked, and the word
//! being typed isn't marked until it's done.

use std::ops::Range;

/// The misspelled words in a text, as byte ranges in order: the system's
/// checker (none off macOS).
pub fn check(text: &str) -> Vec<Range<usize>> {
    #[cfg(target_os = "macos")]
    return macos::check(text);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Vec::new()
    }
}

/// The system's suggestions.
pub fn guesses(text: &str, range: Range<usize>) -> Vec<String> {
    #[cfg(target_os = "macos")]
    return macos::guesses(text, range);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (text, range);
        Vec::new()
    }
}

/// UTF-16 offset of a byte offset (NSString counts UTF-16 units).
fn utf16_at(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

/// Byte offset of a UTF-16 offset, if it falls on a character boundary.
fn byte_at(text: &str, utf16: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units == utf16 {
            return Some(byte);
        }
        units += c.len_utf16();
    }
    (units == utf16).then_some(text.len())
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ops::Range;

    use objc2_app_kit::NSSpellChecker;
    use objc2_foundation::{NSNotFound, NSRange, NSString};

    pub fn check(text: &str) -> Vec<Range<usize>> {
        let string = NSString::from_str(text);
        let checker = NSSpellChecker::sharedSpellChecker();
        let total = string.length();
        let mut found = Vec::new();
        let mut from = 0usize;
        while from < total {
            let range: NSRange = unsafe {
                checker.checkSpellingOfString_startingAt_language_wrap_inSpellDocumentWithTag_wordCount(
                    &string,
                    from as isize,
                    None,
                    false,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if range.location == NSNotFound as usize || range.length == 0 || range.location < from {
                break;
            }
            if let (Some(start), Some(end)) = (super::byte_at(text, range.location), super::byte_at(text, range.location + range.length)) {
                found.push(start..end);
            }
            from = range.location + range.length;
        }
        found
    }

    pub fn guesses(text: &str, range: Range<usize>) -> Vec<String> {
        let string = NSString::from_str(text);
        let start = super::utf16_at(text, range.start);
        let range = NSRange::new(start, super::utf16_at(text, range.end) - start);
        let checker = NSSpellChecker::sharedSpellChecker();
        checker
            .guessesForWordRange_inString_language_inSpellDocumentWithTag(range, &string, None, 0)
            .map(|list| list.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }
}

/// The misspellings to mark: not in a mention or a link (`skip`), and not
/// the word the caret is at the end of while it's still being typed.
pub fn marked(found: &[Range<usize>], skip: &[Range<usize>], caret: Option<usize>) -> Vec<Range<usize>> {
    found
        .iter()
        .filter(|r| !skip.iter().any(|s| s.start < r.end && r.start < s.end))
        .filter(|r| caret != Some(r.end))
        .cloned()
        .collect()
}

/// Misspellings found in `checked`, carried over to `now` (typed since):
/// those before the change stay, those after it move with the text, and
/// those it touched are dropped until the next check.
pub fn carry_over(checked: &str, found: &[Range<usize>], now: &str) -> Vec<Range<usize>> {
    if checked == now {
        return found.to_vec();
    }
    let prefix = checked.bytes().zip(now.bytes()).take_while(|(a, b)| a == b).count();
    let max_suffix = checked.len().min(now.len()) - prefix;
    let suffix = checked.bytes().rev().zip(now.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    let changed_end = checked.len() - suffix;
    let shift = now.len() as isize - checked.len() as isize;
    // A word still ends (or starts) where it did when the text beside it
    // isn't part of a word.
    let word_char = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '\'' || c == '’');
    found
        .iter()
        .filter_map(|r| {
            if r.end < prefix || (r.end == prefix && !word_char(now[prefix..].chars().next())) {
                Some(r.clone())
            } else if r.start > changed_end
                || (r.start == changed_end
                    && !word_char(now[..(r.start as isize + shift).max(0) as usize].chars().next_back()))
            {
                let start = r.start.checked_add_signed(shift)?;
                let end = r.end.checked_add_signed(shift)?;
                (now.is_char_boundary(start) && now.is_char_boundary(end)).then_some(start..end)
            } else {
                None
            }
        })
        .collect()
}

/// The misspelling a right-click at byte `at` lands on.
pub fn at(marked: &[Range<usize>], at: usize) -> Option<Range<usize>> {
    marked.iter().find(|r| r.start <= at && at <= r.end).cloned()
}

#[cfg(test)]
// Ranges of bytes, not lists of them.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_map_to_bytes() {
        let text = "é😀a";
        assert_eq!(utf16_at(text, 2), 1);
        assert_eq!(utf16_at(text, 6), 3);
        assert_eq!(byte_at(text, 3), Some(6));
        assert_eq!(byte_at(text, 2), None, "inside the emoji");
        assert_eq!(byte_at(text, 4), Some(7));
    }

    #[test]
    fn mentions_links_and_the_word_being_typed_arent_marked() {
        let found = [0..4, 10..14, 20..25];
        assert_eq!(marked(&found, &[9..15], None), [0..4, 20..25]);
        assert_eq!(marked(&found, &[], Some(25)), [0..4, 10..14], "still typing it");
        assert_eq!(marked(&found, &[], Some(26)), found);
    }

    #[test]
    fn misspellings_follow_the_text_as_its_typed() {
        let checked = "helo wrld bye";
        let found = [0..4, 5..9];
        assert_eq!(carry_over(checked, &found, checked), found);
        // Typed at the end: both stay.
        assert_eq!(carry_over(checked, &found, "helo wrld bye now"), found);
        // Typed at the start: both move.
        assert_eq!(carry_over(checked, &found, "oh helo wrld bye"), [3..7, 8..12]);
        // The second word fixed: only the first stays.
        assert_eq!(carry_over(checked, &found, "helo world bye"), [0..4]);
        // Typed onto a word: it's checked again.
        assert_eq!(carry_over(checked, &found, "xhelo wrld bye"), [6..10]);
        assert_eq!(carry_over(checked, &found, "helox wrld bye"), [6..10]);
        assert_eq!(at(&[0..4, 8..12], 10), Some(8..12));
        assert_eq!(at(&[0..4], 6), None);
    }

    /// macOS's checker, headless: obvious misspellings are found, and a
    /// word it knows isn't. (Nothing depends on the dictionary beyond that.)
    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_checker_finds_obvious_misspellings() {
        let text = "Thiss is a tesst of the checker";
        let found = check(text);
        let words: Vec<&str> = found.iter().map(|r| &text[r.clone()]).collect();
        assert!(words.contains(&"Thiss") && words.contains(&"tesst"), "{words:?}");
        assert!(!words.contains(&"checker"));
        let range = found.iter().find(|r| &text[(*r).clone()] == "tesst").unwrap().clone();
        assert!(!guesses(text, range).is_empty());
    }
}
