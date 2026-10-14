//! "Link this device" screen with the provisioning QR code.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::prelude::*;
use gpui_kit::{App, Bounds, Entity, FontWeight, Hsla, IntoElement, Window, canvas, div, fill, point, px, size, white};
use qrcode::QrCode;
use signal_backend::Command;

use crate::state::{AppState, Screen};
use crate::theme::Palette;

const QR_SIZE: f32 = 260.;

pub fn render(state: &Entity<AppState>, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let _ = window;
    let palette = Palette::get(cx);
    let (url, error) = match &state.read(cx).screen {
        Screen::Link { url, error } => (url.clone(), error.clone()),
        _ => (None, None),
    };

    let qr: gpui_kit::AnyElement = match (&url, &error) {
        (_, Some(error)) => div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(div().text_color(palette.danger).child(format!("Linking failed: {error}")))
            .child(Button::new("retry").primary().label("Try again").on_click({
                let state = state.clone();
                move |_, _, cx| {
                    state.update(cx, |state, cx| {
                        state.screen = Screen::Link { url: None, error: None };
                        state.send(Command::Link);
                        cx.notify();
                    })
                }
            }))
            .into_any_element(),
        (Some(url), None) => qr_code(&with_history(url.as_str()), palette.text).into_any_element(),
        (None, None) => div()
            .size(px(QR_SIZE))
            .flex()
            .items_center()
            .justify_center()
            .child(Spinner::new())
            .into_any_element(),
    };

    let step = |n: u8, text: &'static str| {
        div()
            .flex()
            .gap_2()
            .child(div().text_color(palette.text_secondary).child(format!("{n}.")))
            .child(text)
    };

    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(palette.background)
        .text_color(palette.text)
        .child(
            div()
                .flex()
                .gap(px(48.))
                .items_center()
                .child(
                    div()
                        .p_4()
                        .rounded_lg()
                        .bg(white())
                        .border_1()
                        .border_color(palette.panel_border)
                        .child(qr),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .max_w(px(360.))
                        .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Scan QR code to link"))
                        .child(step(1, "Open Signal on your phone"))
                        .child(step(2, "Tap Settings → Linked devices"))
                        .child(step(3, "Tap \"Link new device\" and point your phone at this screen"))
                        .child(
                            div()
                                .mt_4()
                                .text_sm()
                                .text_color(palette.text_secondary)
                                .child("Messages are end-to-end encrypted. This device keeps its own encrypted database."),
                        ),
                ),
        )
}

/// Paints `data` as a QR code with one quad per dark module.
fn qr_code(data: &str, _fg: Hsla) -> impl IntoElement {
    let modules: Option<(usize, Vec<bool>)> = QrCode::new(data.as_bytes()).ok().map(|code| {
        let width = code.width();
        let dark = code
            .to_colors()
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect();
        (width, dark)
    });
    canvas(
        move |_, _, _| modules,
        move |bounds: Bounds<gpui_kit::Pixels>, modules, window, _| {
            let Some((width, dark)) = modules else { return };
            // Quiet zone of 2 modules on each side.
            let cell = bounds.size.width / (width + 4) as f32;
            let origin = bounds.origin + point(cell * 2., cell * 2.);
            for (ix, is_dark) in dark.iter().enumerate() {
                if *is_dark {
                    let (x, y) = ((ix % width) as f32, (ix / width) as f32);
                    let quad = Bounds::new(origin + point(cell * x, cell * y), size(cell, cell));
                    // Always black on white: phones scan this, not people.
                    window.paint_quad(fill(quad, gpui_kit::black()));
                }
            }
        },
    )
    .size(px(QR_SIZE))
}
/// Asks the phone to send message history, as Desktop's link QR does
/// (`capabilities=backup5`, "link and sync").
fn with_history(url: &str) -> String {
    if url.contains("capabilities=") {
        url.to_owned()
    } else {
        format!("{url}&capabilities=backup5")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_qr_asks_for_message_history() {
        let url = "sgnl://linkdevice?uuid=abc&pub_key=xyz";
        assert_eq!(with_history(url), "sgnl://linkdevice?uuid=abc&pub_key=xyz&capabilities=backup5");
        assert_eq!(with_history(&with_history(url)), with_history(url));
    }
}
