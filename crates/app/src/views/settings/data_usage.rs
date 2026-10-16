//! Desktop's Data usage page, with this app's lossless image optimization
//! where Desktop has its sent media quality.

use gpui_kit::{AnyElement, App, Entity, SharedString};
use signal_backend::media::Optimization;

use super::{Choice, Settings, list, select};
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let current = state.read(cx).image_optimization;
    let choice = |label: &'static str, level: Optimization| {
        let state = state.clone();
        Choice::new(label, current == level, move |_, cx| state.update(cx, |s, cx| s.set_image_optimization(level, cx)))
    };
    let explanation: SharedString = match current {
        Optimization::Max => {
            "Images you send are made as small as possible without changing a pixel (oxipng for PNG, mozjpeg for \
             JPEG). Large images can take a few seconds."
        }
        Optimization::Fast => "Images are made smaller without changing a pixel, trading some savings for speed.",
        Optimization::Off => "Images are sent as they are, apart from removing metadata such as location.",
    }
    .into();
    list(
        None,
        None,
        [select(
            "image-optimization",
            "Optimize images before sending",
            Some(explanation),
            vec![choice("Maximum", Optimization::Max), choice("Fast", Optimization::Fast), choice("Off", Optimization::Off)],
            false,
            palette,
        )],
        palette,
    )
}
