//! Signal's palette, resolved for the current light/dark appearance.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Hsla, rgb};

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    pub sidebar: Hsla,
    pub panel_border: Hsla,
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub bubble_out: Hsla,
    pub bubble_out_text: Hsla,
    pub bubble_out_meta: Hsla,
    pub bubble_in: Hsla,
    pub bubble_in_text: Hsla,
    pub row_hover: Hsla,
    pub row_selected: Hsla,
    pub input_bg: Hsla,
    pub popover: Hsla,
    pub reaction_chip: Hsla,
    pub reaction_chip_mine: Hsla,
    pub danger: Hsla,
    pub badge: Hsla,
}

impl Palette {
    pub fn get(cx: &App) -> Palette {
        if cx.theme().is_dark() { Self::dark() } else { Self::light() }
    }

    fn light() -> Palette {
        Palette {
            background: c(0xFFFFFF),
            sidebar: c(0xF6F6F6),
            panel_border: c(0xDEDEDE),
            text: c(0x1B1B1B),
            text_secondary: c(0x5E5E5E),
            accent: c(0x2C6BED),
            on_accent: c(0xFFFFFF),
            bubble_out: c(0x2C6BED),
            bubble_out_text: c(0xFFFFFF),
            bubble_out_meta: Hsla { a: 0.8, ..c(0xFFFFFF) },
            bubble_in: c(0xE9E9E9),
            bubble_in_text: c(0x1B1B1B),
            row_hover: c(0xEBEBEB),
            row_selected: c(0xDEDEDE),
            input_bg: c(0xF0F0F0),
            popover: c(0xFFFFFF),
            reaction_chip: c(0xF0F0F0),
            reaction_chip_mine: c(0xD4D4D4),
            danger: c(0xD92D20),
            badge: c(0x2C6BED),
        }
    }

    fn dark() -> Palette {
        Palette {
            background: c(0x121212),
            sidebar: c(0x1B1B1B),
            panel_border: c(0x2E2E2E),
            text: c(0xE9E9E9),
            text_secondary: c(0xA9A9A9),
            accent: c(0x2C6BED),
            on_accent: c(0xFFFFFF),
            bubble_out: c(0x2C6BED),
            bubble_out_text: c(0xFFFFFF),
            bubble_out_meta: Hsla { a: 0.8, ..c(0xFFFFFF) },
            bubble_in: c(0x3B3B3B),
            bubble_in_text: c(0xE9E9E9),
            row_hover: c(0x2A2A2A),
            row_selected: c(0x3B3B3B),
            input_bg: c(0x2A2A2A),
            popover: c(0x2A2A2A),
            reaction_chip: c(0x2E2E2E),
            reaction_chip_mine: c(0x4A4A4A),
            danger: c(0xF04438),
            badge: c(0x2C6BED),
        }
    }
}

/// Signal's avatar colors as (background, foreground) pairs.
const AVATAR_COLORS: [(u32, u32); 12] = [
    (0xE3E3FE, 0x3838F5),
    (0xDDE7FC, 0x1251D3),
    (0xD8E8F0, 0x086DA0),
    (0xCDE4CD, 0x067906),
    (0xEAE0FD, 0x661AFF),
    (0xF5E3FE, 0x9F00F0),
    (0xF6D8EC, 0xB8057C),
    (0xF5D7D7, 0xBE0404),
    (0xFEF5D0, 0x836B01),
    (0xEAE6D5, 0x7D6F40),
    (0xD2D2DC, 0x4F4F6D),
    (0xD7D7D9, 0x5C5C5C),
];

/// Signal's avatar color `index` as (background, foreground) RGB.
pub fn avatar_rgb(index: u8) -> (u32, u32) {
    AVATAR_COLORS[index as usize % AVATAR_COLORS.len()]
}

pub fn avatar_colors(index: u8) -> (Hsla, Hsla) {
    let (bg, fg) = AVATAR_COLORS[index as usize % AVATAR_COLORS.len()];
    (c(bg), c(fg))
}

/// Avatar color for an account (service id) or conversation id, by
/// Signal's hash so people get the same color as in Signal Desktop.
pub fn color_for(key: &str) -> u8 {
    signal_store::avatar_color(key.strip_prefix("d:").unwrap_or(key))
}

/// Signal Desktop's sender-name colors (`$contact-colors`): (light, dark),
/// in the order Desktop hands them out (`ContactNameColors`).
const CONTACT_NAME_COLORS: [(u32, u32); 36] = [
    (0x006da3, 0x00a7fa), // 200
    (0x067906, 0x0ab80a), // 120
    (0xb814b8, 0xf65af6), // 300
    (0xc13215, 0xff6f52), // 010
    (0x5b6976, 0x8ba1b6), // 210
    (0xcc0066, 0xf76eb2), // 330
    (0x2e51ff, 0x8599ff), // 230
    (0x007575, 0x00b2b2), // 180
    (0x9c5711, 0xd5920b), // 030
    (0xd00b4d, 0xff6b9c), // 340
    (0x8f2af4, 0xbf80ff), // 270
    (0x3d7406, 0x5eb309), // 090
    (0xd00b0b, 0xff7070), // 000
    (0x007a3d, 0x00b85c), // 150
    (0x5151f6, 0x9494ff), // 240
    (0x866118, 0xd68f00), // 040
    (0x067953, 0x00b87a), // 160
    (0xa20ced, 0xcf7cf8), // 280
    (0x4b7000, 0x74ad00), // 080
    (0xc70a88, 0xf76ec9), // 320
    (0xb34209, 0xf57a3d), // 020
    (0x06792d, 0x0ab844), // 140
    (0x7a3df5, 0xaf8af9), // 260
    (0x6b6b24, 0xa4a437), // 060
    (0xd00b2c, 0xf77389), // 350
    (0x2d7906, 0x42b309), // 100
    (0xaf0bd0, 0xe06ef7), // 290
    (0x32763e, 0x4baf5c), // 130
    (0x2662d9, 0x7da1e8), // 220
    (0x76681e, 0xb89b0a), // 050
    (0x067462, 0x09b397), // 170
    (0x6447f5, 0xa18ff9), // 250
    (0x5e6e0c, 0x8faa09), // 070
    (0x077288, 0x00aed1), // 190
    (0xc20aa3, 0xf75fdd), // 310
    (0x2d761e, 0x43b42d), // 110
];

/// A sender's name color in a group, as Signal Desktop picks it: the
/// member's place among the group's members sorted by ACI. Unknown senders
/// get the first color.
pub fn name_color(members: &[String], author: &str, dark: bool) -> Hsla {
    let mut sorted: Vec<&String> = members.iter().collect();
    sorted.sort();
    let index = sorted.iter().position(|m| m.as_str() == author).unwrap_or(0);
    let (light, dark_value) = CONTACT_NAME_COLORS[index % CONTACT_NAME_COLORS.len()];
    c(if dark { dark_value } else { light })
}

/// Up to two initials for an avatar.
pub fn initials(name: &str) -> String {
    if name.trim_start().starts_with('+') {
        return "#".into(); // phone number
    }
    let mut words = name.split_whitespace().filter(|w| w.chars().next().is_some_and(char::is_alphanumeric));
    let first = words.next().and_then(|w| w.chars().next());
    let last = words.next_back().and_then(|w| w.chars().next());
    match (first, last) {
        (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "#".into(),
    }
}

#[cfg(test)]
mod name_color_tests {
    use super::*;

    #[test]
    fn names_follow_member_order_like_desktop() {
        let members = vec!["c".to_owned(), "a".to_owned(), "b".to_owned()];
        // Sorted: a, b, c → Desktop's first three name colors.
        assert_eq!(name_color(&members, "a", true), c(0x00a7fa));
        assert_eq!(name_color(&members, "b", true), c(0x0ab80a));
        assert_eq!(name_color(&members, "c", false), c(0xb814b8));
        // Not a member (left the group): the first color.
        assert_eq!(name_color(&members, "z", true), c(0x00a7fa));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_work() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("ada"), "A");
        assert_eq!(initials("Grace Brewster Hopper"), "GH");
        assert_eq!(initials("+1 555 0100"), "#");
        assert_eq!(initials(""), "#");
    }
}
