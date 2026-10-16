//! Desktop's Appearance page: the theme and the zoom level. (Its language
//! and chat color choices aren't here.)

use gpui_kit::{AnyElement, App, Entity};

use super::{Choice, Settings, list, select};
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
    list(
        None,
        None,
        [select("theme", "Theme", None, themes, false, palette), select("zoom", "Zoom level", None, zooms, false, palette)],
        palette,
    )
}
