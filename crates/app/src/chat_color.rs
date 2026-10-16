//! Chat colors as Signal Desktop draws them: its presets
//! (`AxoTokens.ChatBubble` in `ts/axo/AxoTokens.std.ts`), custom colors
//! through `getHSL` (`ts/util/getHSL.std.ts`), and gradients as
//! `getChatBubbleColorData` turns them into CSS.

use gpui_kit::{Background, Hsla, linear_color_stop, linear_gradient, rgb};
use signal_store::{ChatColor, CustomColor, HueSaturation};

/// A bubble's fill: one color, or a CSS gradient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BubbleFill {
    Solid(Hsla),
    /// CSS angle (0: to the top, clockwise), start and end colors.
    Gradient(f32, Hsla, Hsla),
}

impl BubbleFill {
    pub fn background(self) -> Background {
        match self {
            Self::Solid(color) => color.into(),
            Self::Gradient(deg, start, end) => linear_gradient(deg, linear_color_stop(start, 0.), linear_color_stop(end, 1.)),
        }
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

/// Desktop's preset `name`, or none for a name it doesn't have.
pub fn preset(name: &str) -> Option<BubbleFill> {
    let solid = |v| Some(BubbleFill::Solid(hex(v)));
    let gradient = |deg, start, end| Some(BubbleFill::Gradient(deg, hex(start), hex(end)));
    match name {
        "crimson" => solid(0xcf163e),
        "vermilion" => solid(0xc73f0a),
        "burlap" => solid(0x6f6a58),
        "forest" => solid(0x3b7845),
        "wintergreen" => solid(0x1d8663),
        "teal" => solid(0x077d92),
        "blue" => solid(0x336ba3),
        "indigo" => solid(0x6058ca),
        "violet" => solid(0x9932c8),
        "plum" => solid(0xaa377a),
        "taupe" => solid(0x8f616a),
        "steel" => solid(0x71717f),
        "ultramarine" => gradient(180., 0x0552f0, 0x2c6bed),
        "basil" => gradient(180., 0x2f9373, 0x077343),
        "ember" => gradient(168., 0xe57c00, 0x5e0000),
        "fluorescent" => gradient(192., 0xec13dd, 0x1b36c6),
        "infrared" => gradient(192., 0xf65560, 0x442ced),
        "lagoon" => gradient(180., 0x004066, 0x32867d),
        "midnight" => gradient(180., 0x2c2c3a, 0x787891),
        "sea" => gradient(180., 0x498fd4, 0x2c66a0),
        "sublime" => gradient(180., 0x6281d5, 0x974460),
        "tangerine" => gradient(192., 0xdb7133, 0x911231),
        _ => None,
    }
}

/// Desktop's `calculateLightness`: how light a hue is drawn, in percent.
pub fn lightness_for(hue: f32) -> f32 {
    let between = |hue: f32, (min, from): (f32, f32), (max, to): (f32, f32)| from + (hue - min) / (max - min) * (to - from);
    match hue {
        h if h < 60. => between(h, (0., 45.), (60., 30.)),
        h if h < 180. => 30.,
        h if h < 240. => between(h, (180., 30.), (240., 50.)),
        h if h < 300. => between(h, (240., 50.), (300., 40.)),
        h => between(h, (300., 40.), (360., 45.)),
    }
}

/// Desktop's `getHSL`: a custom color's end as a color, its lightness from
/// the hue unless it came with one.
pub fn hsl(color: HueSaturation) -> Hsla {
    let lightness = color.lightness.map_or_else(|| lightness_for(color.hue), |l| l * 100.);
    Hsla { h: color.hue.rem_euclid(360.) / 360., s: (color.saturation / 100.).clamp(0., 1.), l: (lightness / 100.).clamp(0., 1.), a: 1. }
}

/// Desktop's `getChatBubbleColorData` for a custom color: its gradient at
/// `270 - deg` in CSS, or its one color.
pub fn custom(color: &CustomColor) -> BubbleFill {
    match color.end {
        Some(end) => BubbleFill::Gradient((270. - color.deg.unwrap_or(0.)).rem_euclid(360.), hsl(color.start), hsl(end)),
        None => BubbleFill::Solid(hsl(color.start)),
    }
}

/// What a chat color fills our bubbles with; a preset Desktop doesn't
/// have falls back to ultramarine, its default.
pub fn fill(color: &ChatColor) -> BubbleFill {
    match color {
        ChatColor::Preset { name } => preset(name).or_else(|| preset("ultramarine")).expect("ultramarine is a preset"),
        ChatColor::Custom { value, .. } => custom(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_desktops_colors() {
        for name in signal_store::CHAT_COLOR_PRESETS {
            assert!(preset(name).is_some(), "{name}");
        }
        assert_eq!(preset("crimson"), Some(BubbleFill::Solid(hex(0xcf163e))));
        assert_eq!(fill(&ChatColor::preset("nonsense")), preset("ultramarine").unwrap());
    }

    #[test]
    fn custom_colors_follow_get_hsl() {
        // Desktop's lightness table: 45% at red, 30% from yellow to cyan,
        // 50% at blue.
        assert_eq!(lightness_for(0.), 45.);
        assert_eq!(lightness_for(120.), 30.);
        assert_eq!(lightness_for(240.), 50.);
        assert_eq!(lightness_for(210.), 40.);
        let solid = CustomColor { start: HueSaturation { hue: 240., saturation: 50., lightness: None }, end: None, deg: None };
        assert_eq!(custom(&solid), BubbleFill::Solid(Hsla { h: 240. / 360., s: 0.5, l: 0.5, a: 1. }));
        // A color from a backup keeps its lightness.
        let kept = HueSaturation { hue: 0., saturation: 100., lightness: Some(0.2) };
        assert!((hsl(kept).l - 0.2).abs() < 1e-6);
        let gradient = CustomColor { end: Some(kept), deg: Some(90.), ..solid };
        assert!(matches!(custom(&gradient), BubbleFill::Gradient(deg, _, _) if deg == 180.));
    }
}
