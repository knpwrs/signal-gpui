//! Desktop's General page: the account, "Open at computer login", and (as
//! a linked device has it) deleting this device's data; plus the debug log
//! and About, which Desktop keeps in its Help menu.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::{AnyElement, App, Entity, IntoElement as _, ParentElement as _, Styled as _, Window, div};

use super::{Settings, action, list, switch, text};
use crate::state::AppState;
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, _window: &mut Window, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let (account, open_at_login) = {
        let s = state.read(cx);
        (s.account.clone(), s.open_at_login)
    };
    let phone = account.as_ref().and_then(|a| a.phone.clone()).unwrap_or_else(|| "Unknown".into());
    let aci = account.map(|a| a.aci).unwrap_or_default();
    let login_state = state.clone();
    let (unlink_state, log_state) = (state.clone(), state.clone());
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(list(None, None, [text("Phone Number", phone, palette), text("Account ID", aci, palette)], palette))
        .child(list(
            Some("System"),
            None,
            [switch("open-at-login", "Open at computer login", None, open_at_login, false, palette, move |on, _, cx| {
                login_state.update(cx, |s, cx| s.set_open_at_login(on, cx));
            })],
            palette,
        ))
        .child(list(
            None,
            None,
            [action(
                "Unlink this device",
                Some(
                    "This computer is linked to your phone. Unlinking signs it out, removes its keys, and deletes the \
                     messages and media stored on this computer."
                        .into(),
                ),
                Button::new("unlink")
                    .danger()
                    .label("Unlink…")
                    .on_click(move |_, window, cx| confirm_unlink(unlink_state.clone(), window, cx)),
                palette,
            )],
            palette,
        ))
        .child(list(
            Some("Help"),
            None,
            [
                action(
                    "Debug log",
                    None,
                    Button::new("debug-log")
                        .outline()
                        .label("Show…")
                        .on_click(move |_, window, cx| crate::views::debug_log::show(log_state.clone(), window, cx)),
                    palette,
                ),
                action(
                    "About & licenses",
                    None,
                    Button::new("about").outline().label("Show…").on_click(|_, window, cx| crate::views::about::show(window, cx)),
                    palette,
                ),
            ],
            palette,
        ))
        .into_any_element()
}

fn confirm_unlink(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .title("Unlink this device?")
            .description("Messages and media on this computer will be deleted. You'll need to scan a new QR code from your phone to use Signal here again.")
            .confirm()
            .ok_text("Unlink")
            .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.read(cx).send(signal_backend::Command::Unlink);
                window.close_all_dialogs(cx);
                true
            })
    });
}
