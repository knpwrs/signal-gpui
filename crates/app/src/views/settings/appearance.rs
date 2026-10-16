//! Desktop's Appearance page: the theme, the chat color and the zoom
//! level. (Its language choice isn't here.)

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, TestSupportExt as _, div};

use super::{Choice, Page, Settings, label_block, list, select};
use crate::prefs::{ThemeSetting, ZOOM_FACTORS, zoom_label};
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let (theme, zoom) = {
        let prefs = &state.read(cx).prefs;
        (prefs.theme, prefs.zoom)
    };
    let themes = ThemeSetting::ALL
        .into_iter()
        .map(|choice| {
            let state = state.clone();
            Choice::new(choice.label(), choice == theme, move |_, cx| state.update(cx, |s, cx| s.set_theme(choice, cx)))
        })
        .collect();
    // A zoom set elsewhere that isn't one of Desktop's choices is listed
    // too, as Desktop adds it.
    let mut factors = ZOOM_FACTORS.to_vec();
    if !factors.iter().any(|f| (f - zoom).abs() < 0.001) {
        factors.push(zoom);
        factors.sort_by(f32::total_cmp);
    }
    let zooms = factors
        .into_iter()
        .map(|factor| {
            let state = state.clone();
            Choice::new(zoom_label(factor), (factor - zoom).abs() < 0.001, move |_, cx| state.update(cx, |s, cx| s.set_zoom(factor, cx)))
        })
        .collect();
    // Desktop's "Chat color" row, with the default's swatch.
    let default = state.read(cx).chat_colors.default.clone();
    let open = settings.clone();
    let chat_color = div()
        .id("chat-color")
        .flex()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .on_click(move |_, window, cx| super::go(&open, Page::ChatColor, window, cx))
        .child(label_block("Chat color", None, palette))
        .child(crate::views::chat_colors::swatch_for(&default))
        .child(div().text_color(palette.text_secondary).child("›"))
        .test_support()
        .into_any_element();
    list(
        None,
        None,
        [select("theme", "Theme", None, themes, false, palette), chat_color, select("zoom", "Zoom level", None, zooms, false, palette)],
        palette,
    )
}
