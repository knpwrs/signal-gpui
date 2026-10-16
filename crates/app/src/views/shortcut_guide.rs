//! Desktop's keyboard shortcut guide (`ShortcutGuideModal`, from Help →
//! Show Keyboard Shortcuts or ⌘/): each section's shortcuts with their keys
//! drawn as key caps, square for single symbols as Desktop draws them.

use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{App, FontWeight, SharedString, Window, div, px};

use crate::shortcuts::GUIDE;
use crate::theme::Palette;

/// Opens the guide, unless a dialog (the guide among them) is up.
pub fn open(window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    window.open_dialog(cx, |dialog, _, cx| {
        let palette = Palette::get(cx);
        dialog.title("Keyboard Shortcuts").w(px(560.)).child(
            div()
                .id("shortcut-guide")
                .debug_selector(|| "shortcut-guide".into())
                .max_h(px(520.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .children(GUIDE.iter().map(|(section, shortcuts)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .pb_1()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(palette.text)
                                .child(*section),
                        )
                        .children(shortcuts.iter().map(|shortcut| {
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .py_1()
                                .text_sm()
                                .child(div().flex_1().min_w_0().text_color(palette.text).child(shortcut.description))
                                .child(div().flex().flex_none().gap_3().children(shortcut.keys.iter().map(|keys| {
                                    div().flex().gap_1().children(keys.iter().map(|key| key_cap(key, palette)))
                                })))
                        }))
                })),
        )
    });
}

/// A key: square for one symbol or letter, wider for a name.
fn key_cap(label: &'static str, palette: Palette) -> impl IntoElement {
    let square = label.chars().count() == 1;
    div()
        .h(px(24.))
        .when(square, |d| d.w(px(24.)))
        .when(!square, |d| d.px_2())
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(palette.panel_border)
        .bg(palette.input_bg)
        .text_xs()
        .text_color(palette.text)
        .child(SharedString::from(label))
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use crate::views::test_support::{demo, shows};

    #[gpui_kit::test]
    fn the_guide_opens_once(cx: &mut TestAppContext) {
        let (backend, _state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        assert!(!shows(cx, "shortcut-guide"));
        cx.update(super::open);
        assert!(shows(cx, "shortcut-guide"));
        // Asking again while it's up doesn't stack a second one.
        cx.update(super::open);
        assert!(cx.update(|window, cx| {
            use gpui_kit::component::WindowExt as _;
            window.has_active_dialog(cx)
        }));
        backend.shutdown();
    }
}
