//! Signal Desktop's fonts for text stories (`stylesheets/_fontfaces.scss`,
//! `ts/util/getFontNameByTextScript`): Inter for the regular and bold
//! styles, EB Garamond for serif, Parisienne for script and Barlow
//! Condensed for condensed. All are under the SIL Open Font License; their
//! notices are next to them in `assets/fonts`.

use std::borrow::Cow;

use gpui_kit::App;

/// The family names, as the files name themselves.
pub const INTER: &str = "Inter";
pub const EB_GARAMOND: &str = "EB Garamond";
pub const PARISIENNE: &str = "Parisienne";
/// Barlow Condensed Medium names its family with its weight.
pub const BARLOW_CONDENSED: &str = "Barlow Condensed Medium";

/// The font files, for the app's text system.
pub const FILES: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/Inter-Regular.otf"),
    include_bytes!("../assets/fonts/Inter-Bold.otf"),
    include_bytes!("../assets/fonts/EBGaramond-Regular.ttf"),
    include_bytes!("../assets/fonts/Parisienne-Regular.ttf"),
    include_bytes!("../assets/fonts/BarlowCondensed-Medium.ttf"),
];

/// Adds the fonts to the app's text system.
pub fn register(cx: &mut App) {
    let fonts = FILES.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(error = %format!("{error:#}"), "couldn't load the story fonts");
    }
}

#[cfg(test)]
mod tests {
    /// A font's family name (name table entry 1, Windows platform), as the
    /// text system lists it.
    fn family(font: &[u8]) -> Option<String> {
        let u16_at = |at: usize| Some(u16::from_be_bytes(font.get(at..at + 2)?.try_into().ok()?));
        let u32_at = |at: usize| Some(u32::from_be_bytes(font.get(at..at + 4)?.try_into().ok()?));
        let tables = u16_at(4)? as usize;
        let name = (0..tables).map(|i| 12 + 16 * i).find(|at| font.get(*at..*at + 4) == Some(b"name"))?;
        let table = u32_at(name + 8)? as usize;
        let (count, strings) = (u16_at(table + 2)? as usize, u16_at(table + 4)? as usize);
        (0..count).map(|i| table + 6 + 12 * i).find_map(|record| {
            if u16_at(record)? != 3 || u16_at(record + 6)? != 1 {
                return None;
            }
            let (len, offset) = (u16_at(record + 8)? as usize, u16_at(record + 10)? as usize);
            let start = table + strings + offset;
            let units: Vec<u16> =
                font.get(start..start + len)?.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16(&units).ok()
        })
    }

    #[test]
    fn the_story_fonts_are_named_as_the_stories_ask_for_them() {
        let names: Vec<_> = super::FILES.iter().map(|f| family(f)).collect();
        let want = [super::INTER, super::INTER, super::EB_GARAMOND, super::PARISIENNE, super::BARLOW_CONDENSED];
        assert_eq!(names, want.map(|n| Some(n.to_owned())));
    }
}
