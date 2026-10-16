//! Nicknames and notes on contacts, as Signal Desktop keeps them
//! (`nicknameGivenName`, `nicknameFamilyName` and `note` on a 1:1
//! conversation, synced through the Storage Service contact record).

use unicode_segmentation::UnicodeSegmentation as _;

/// Length limits for a nickname's first and last name and for a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NicknameLimits {
    pub name_graphemes: usize,
    pub name_bytes: usize,
    pub note_graphemes: usize,
    pub note_bytes: usize,
}

/// Desktop's limits, hard-coded in `ts/components/EditNicknameAndNoteModal.dom.tsx`
/// (`maxGraphemes={26} maxBytes={128}` for each name, 240 and 240 for the
/// note). No remote config key sets them. The similarly named
/// `global.nicknames.min` / `global.nicknames.max` are the length of a
/// username's nickname part (`ts/util/Username.dom.ts`, defaults 3 and 32),
/// which `signal_backend::recipients::username_in` checks, not these.
pub const NICKNAME_LIMITS: NicknameLimits = NicknameLimits { name_graphemes: 26, name_bytes: 128, note_graphemes: 240, note_bytes: 240 };

/// A contact's nickname and note.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactNickname {
    pub given: String,
    pub family: String,
    pub note: String,
    /// The name they'd have without the nickname: their address-book or
    /// profile name, when known (Desktop's `titleNoNickname`).
    pub plain_name: Option<String>,
}

impl ContactNickname {
    /// The nickname as a name (Desktop's `getNicknameName`).
    pub fn nickname(&self) -> Option<String> {
        combine_names(&self.given, &self.family)
    }

    /// No nickname and no note.
    pub fn is_empty(&self) -> bool {
        self.given.trim().is_empty() && self.family.trim().is_empty() && self.note.trim().is_empty()
    }
}

/// A given and a family name as one (Desktop's `combineNames`): the given
/// name first, unless both are written in Chinese, Japanese or Korean
/// script, where the family name comes first with no space.
pub fn combine_names(given: &str, family: &str) -> Option<String> {
    let (given, family) = (given.trim(), family.trim());
    match (given.is_empty(), family.is_empty()) {
        (true, true) => None,
        (true, false) => Some(family.to_owned()),
        (false, true) => Some(given.to_owned()),
        (false, false) if all_cjkv(family) && all_cjkv(given) => Some(format!("{family}{given}")),
        (false, false) => Some(format!("{given} {family}")),
    }
}

/// Desktop's `isAllCKJV`: the Unicode blocks it lists, plus spaces.
fn all_cjkv(name: &str) -> bool {
    name.chars().all(|c| {
        matches!(
            c as u32,
            0x20
                | 0x1100..=0x11FF
                | 0x2E80..=0x2EFF
                | 0x2F00..=0x2FFF
                | 0x3000..=0x303F
                | 0x3040..=0x30FF
                | 0x3130..=0x318F
                | 0x31C0..=0x31FF
                | 0x3200..=0x33FF
                | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xA960..=0xA97F
                | 0xAC00..=0xD7FF
                | 0xF900..=0xFAFF
                | 0xFE30..=0xFE4F
                | 0x17000..=0x18AFF
                | 0x1B170..=0x1B2FB
                | 0x20000..=0x3134F
        )
    })
}

/// `text` cut to at most `graphemes` characters and `bytes` UTF-8 bytes,
/// at a character boundary (Desktop's `truncateBytesAndGraphemes`).
pub fn truncate_text(text: &str, graphemes: usize, bytes: usize) -> &str {
    let mut end = 0;
    for (count, (start, grapheme)) in text.grapheme_indices(true).enumerate() {
        if count >= graphemes || start + grapheme.len() > bytes {
            break;
        }
        end = start + grapheme.len();
    }
    &text[..end]
}

/// A nickname and note trimmed and cut to `NICKNAME_LIMITS`, as Desktop's
/// fields keep them (`toOptionalStringValue` trims), without a plain name.
pub fn limit_nickname(nickname: &ContactNickname) -> ContactNickname {
    let limits = NICKNAME_LIMITS;
    let cut = |text: &str, graphemes, bytes| truncate_text(text.trim(), graphemes, bytes).trim().to_owned();
    ContactNickname {
        given: cut(&nickname.given, limits.name_graphemes, limits.name_bytes),
        family: cut(&nickname.family, limits.name_graphemes, limits.name_bytes),
        note: cut(&nickname.note, limits.note_graphemes, limits.note_bytes),
        plain_name: None,
    }
}

/// What Desktop's field counter shows (`AxoBaseField.Count`): characters or
/// bytes left, whichever runs out first, only once half or less is left;
/// and whether to warn (a quarter or less).
pub fn remaining_count(text: &str, graphemes: usize, bytes: usize) -> Option<(usize, bool)> {
    let left_chars = graphemes.saturating_sub(text.graphemes(true).count());
    let left_bytes = bytes.saturating_sub(text.len());
    let (left, max) = if left_bytes > left_chars { (left_chars, graphemes) } else { (left_bytes, bytes) };
    (left * 2 <= max).then_some((left, left * 4 <= max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_combine_as_desktop_combines_them() {
        assert_eq!(combine_names(" Jo ", ""), Some("Jo".into()));
        assert_eq!(combine_names("", "Reyes"), Some("Reyes".into()));
        assert_eq!(combine_names("Jo", "Reyes"), Some("Jo Reyes".into()));
        assert_eq!(combine_names("", "  "), None);
        // Both CJK: family first, no space.
        assert_eq!(combine_names("小明", "王"), Some("王小明".into()));
        assert_eq!(combine_names("민준", "김"), Some("김민준".into()));
        // Mixed scripts keep the usual order.
        assert_eq!(combine_names("Ming", "王"), Some("Ming 王".into()));
    }

    #[test]
    fn truncation_counts_graphemes_and_bytes() {
        let limits = NICKNAME_LIMITS;
        let long = "a".repeat(30);
        assert_eq!(truncate_text(&long, limits.name_graphemes, limits.name_bytes).len(), 26);
        // 26 family emoji are far over 128 bytes: cut by bytes, whole emoji only.
        let family = "👨‍👩‍👧".repeat(26);
        let cut = truncate_text(&family, limits.name_graphemes, limits.name_bytes);
        assert!(cut.len() <= 128 && cut.len().is_multiple_of("👨‍👩‍👧".len()) && !cut.is_empty());
        assert_eq!(truncate_text("short", 26, 128), "short");
        let limited = limit_nickname(&ContactNickname {
            given: format!("  {}", "b".repeat(40)),
            family: " Diaz ".into(),
            note: "n".repeat(300),
            plain_name: Some("ignored".into()),
        });
        assert_eq!((limited.given.len(), limited.family.as_str(), limited.note.len(), limited.plain_name), (26, "Diaz", 240, None));
    }

    #[test]
    fn the_counter_shows_once_half_is_used() {
        assert_eq!(remaining_count("", 26, 128), None);
        assert_eq!(remaining_count(&"a".repeat(12), 26, 128), None);
        assert_eq!(remaining_count(&"a".repeat(13), 26, 128), Some((13, false)));
        assert_eq!(remaining_count(&"a".repeat(20), 26, 128), Some((6, true)));
        assert_eq!(remaining_count(&"a".repeat(26), 26, 128), Some((0, true)));
        // Bytes run out first: counted in bytes.
        assert_eq!(remaining_count(&"é".repeat(100), 240, 240), Some((40, true)));
    }

    #[test]
    fn a_nickname_is_its_names_combined() {
        let nickname = ContactNickname { given: "Mo".into(), family: String::new(), note: String::new(), plain_name: None };
        assert_eq!(nickname.nickname(), Some("Mo".into()));
        assert!(!nickname.is_empty());
        assert!(ContactNickname::default().is_empty());
    }
}
