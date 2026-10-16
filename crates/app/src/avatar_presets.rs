//! Signal Desktop's preset profile photos (`PersonalAvatarIcons` and
//! `personalIconColors` in `ts/types/Avatar.std.ts`): its illustrations
//! (`images/avatars`, AGPL-3.0-only like this app) on one of Signal's
//! avatar colors, drawn as Desktop's `avatarDataToBytes` draws them: the
//! color filling a 1024-pixel square, the illustration over it.

/// Desktop's canvas size (`CANVAS_SIZE`).
const SIZE: u32 = 1024;

/// A preset: the illustration and its color (an index into the avatar
/// colors, A100 = 0 to A210 = 11).
pub struct Preset {
    pub svg: &'static [u8],
    pub color: u8,
}

macro_rules! preset {
    ($name:literal, $color:literal) => {
        Preset { svg: include_bytes!(concat!("../assets/avatars/avatar_", $name, ".svg")), color: $color }
    };
}

/// In Desktop's order, with Desktop's colors.
pub const PRESETS: [Preset; 12] = [
    preset!("abstract_01", 3),
    preset!("abstract_02", 2),
    preset!("abstract_03", 7),
    preset!("cat", 9),
    preset!("dog", 4),
    preset!("fox", 9),
    preset!("tucan", 2),
    preset!("pig", 6),
    preset!("dinosour", 3),
    preset!("sloth", 8),
    preset!("incognito", 11),
    preset!("ghost", 0),
];

/// The preset as a PNG, its color behind the illustration.
pub fn render(preset: &Preset) -> anyhow::Result<Vec<u8>> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(preset.svg, &usvg::Options::default())?;
    let mut pixmap = tiny_skia::Pixmap::new(SIZE, SIZE).ok_or_else(|| anyhow::anyhow!("no pixmap"))?;
    let bg = crate::theme::avatar_rgb(preset.color).0;
    pixmap.fill(tiny_skia::Color::from_rgba8((bg >> 16) as u8, (bg >> 8) as u8, bg as u8, 255));
    let scale = SIZE as f32 / tree.size().width().max(tree.size().height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok(pixmap.encode_png()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_preset_draws_on_its_color() {
        for (ix, preset) in super::PRESETS.iter().enumerate() {
            let png = super::render(preset).unwrap();
            let image = image::load_from_memory(&png).unwrap().to_rgba8();
            assert_eq!(image.dimensions(), (1024, 1024), "preset {ix}");
            assert!(image.pixels().all(|p| p.0[3] == 255), "preset {ix} is opaque");
        }
        // The cat sits on A190's background in a corner.
        let png = super::render(&super::PRESETS[3]).unwrap();
        let corner = image::load_from_memory(&png).unwrap().to_rgba8().get_pixel(0, 0).0;
        assert_eq!(corner, [0xea, 0xe6, 0xd5, 0xff]);
    }
}
