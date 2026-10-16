//! Desktop's captcha dialog (`CaptchaDialog.dom.tsx`), its idle-phone
//! warnings (`WarningIdlePrimaryDeviceDialog`,
//! `CriticalIdlePrimaryDeviceDialog` and the critical modal) and its clock
//! skew warning (`DialogClockSkew`), in Desktop's words.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, Window, div, px};
use signal_backend::server_alerts::ServerAlert;

use crate::state::AppState;
use crate::theme::Palette;

/// Desktop's support pages for the two warnings.
const IDLE_SUPPORT: &str = "https://support.signal.org/hc/articles/9021007554074-Open-Signal-on-your-phone-to-keep-your-account-active";
const CRITICAL_SUPPORT: &str =
    "https://support.signal.org/hc/articles/8997185514138-Re-connect-your-primary-device-to-continue-using-Signal-Desktop";

/// "Verify to continue messaging": Continue opens the captcha in the
/// browser, which comes back through a `signalcaptcha://` link; closing
/// asks whether to skip (Desktop's two dialogs).
pub fn open_captcha(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let pending = state.read(cx).captcha_pending;
        let (skip_state, open_state) = (state.clone(), state.clone());
        dialog
            .title("Verify to continue messaging")
            .w(px(420.))
            .child(
                div()
                    .debug_selector(|| "captcha-dialog".into())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_sm()
                    .text_color(palette.text_secondary)
                    .child("To help prevent spam on Signal, please complete verification.")
                    .child("After verifying, you can continue messaging. Any paused messages will automatically be sent."),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("captcha-skip").label("Skip verification").outline().on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        confirm_skip(skip_state.clone(), window, cx);
                    }))
                    .child(
                        div().debug_selector(|| "captcha-continue".into()).child(
                            Button::new("captcha-continue").label("Continue").primary().loading(pending).on_click(move |_, _, cx| {
                                open_state.update(cx, |state, cx| {
                                    state.captcha_pending = true;
                                    cx.notify();
                                });
                                cx.open_url(signal_backend::server_alerts::CAPTCHA_URL);
                            }),
                        ),
                    ),
            )
    });
}

/// "Continue without verifying?": skipping fails the held messages.
fn confirm_skip(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let (skip, back) = (state.clone(), state.clone());
        dialog
            .title("Continue without verifying?")
            .w(px(420.))
            .child(
                div()
                    .text_sm()
                    .text_color(palette.text_secondary)
                    .child("If you choose to skip verification, you may miss messages from other people and your messages may fail to send."),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("captcha-skip-cancel").label("Cancel").outline().on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        open_captcha(back.clone(), window, cx);
                    }))
                    .child(Button::new("captcha-skip-confirm").label("Skip verification").danger().on_click(move |_, window, cx| {
                        skip.update(cx, |state, _| {
                            state.captcha_pending = false;
                            state.send(signal_backend::Command::SkipCaptcha);
                        });
                        window.close_all_dialogs(cx);
                    })),
            )
    });
}

/// Desktop's critical idle-phone modal ("Account action required").
pub fn open_critical_modal(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let close = state.clone();
        dialog
            .title("Account action required")
            .w(px(420.))
            .child(div().text_sm().text_color(palette.text_secondary).child(
                "You must open Signal on your phone to continue using this account. If you do not open Signal on your phone, your account will be deleted soon.",
            ))
            .footer(
                DialogFooter::new()
                    .child(Button::new("critical-learn-more").label("Learn more").outline().on_click(|_, _, cx| cx.open_url(CRITICAL_SUPPORT)))
                    .child(Button::new("critical-ok").label("OK").primary().on_click(move |_, window, cx| {
                        close.read(cx).send(signal_backend::Command::DismissServerAlert(ServerAlert::CriticalIdlePrimaryDevice));
                        window.close_all_dialogs(cx);
                    })),
            )
    });
}

/// The warning at the top of the chat list (Desktop's `LeftPaneDialog`s):
/// the clock skew (red) first, else the server's alert.
pub fn left_pane_dialog(state: &Entity<AppState>, palette: Palette, cx: &App) -> Option<AnyElement> {
    let app = state.read(cx);
    let card = |danger: bool| {
        div()
            .mx_3()
            .mb_2()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(if danger { palette.danger.opacity(0.15) } else { palette.row_hover })
            .flex()
            .items_start()
            .gap_2()
            .text_sm()
    };
    if app.clock_skew {
        // `icu:clockSkewDetected`, then the device's date and time.
        let now = chrono::Local::now().format("%b %-d, %Y, %-I:%M:%S %p %Z").to_string();
        return Some(
            card(true)
                .debug_selector(|| "clock-skew".into())
                .child(Icon::new(IconName::TriangleAlert).size_4().flex_none().text_color(palette.danger))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child("Your device date is inaccurate. Adjust your clock and try again. Your device date and time is:")
                        .child(now),
                )
                .into_any_element(),
        );
    }
    let now = chrono::Utc::now().timestamp_millis();
    match signal_backend::server_alerts::to_show(&app.server_alerts, now)? {
        ServerAlert::CriticalIdlePrimaryDevice => Some(
            card(false)
                .id("critical-idle-alert")
                .debug_selector(|| "critical-idle-alert".into())
                .child(Icon::new(IconName::TriangleAlert).size_4().flex_none().text_color(palette.danger))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("Open Signal on your phone"))
                        .child("Your account will be deleted soon unless you open Signal on your phone. This message will go away if you've done it successfully.")
                        .child(div().text_color(palette.accent).cursor_pointer().child("Learn more")),
                )
                .cursor_pointer()
                .on_click(|_, _, cx| cx.open_url(CRITICAL_SUPPORT))
                .into_any_element(),
        ),
        ServerAlert::IdlePrimaryDevice => {
            // It can be closed once (Desktop's `isDismissable`).
            let closable = app.server_alerts.get(&ServerAlert::IdlePrimaryDevice).is_some_and(|i| i.dismissed_at.is_none());
            let close = state.clone();
            Some(
                card(false)
                    .debug_selector(|| "idle-alert".into())
                    .child(Icon::new(IconName::CircleAlert).size_4().flex_none().text_color(palette.text_secondary))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child("Open Signal on your phone to keep your account active")
                            .child(
                                div()
                                    .id("idle-alert-learn-more")
                                    .text_color(palette.accent)
                                    .cursor_pointer()
                                    .on_click(|_, _, cx| cx.open_url(IDLE_SUPPORT))
                                    .child("Learn more"),
                            ),
                    )
                    .when(closable, |d| {
                        d.child(
                            div()
                                .id("idle-alert-close")
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    close.read(cx).send(signal_backend::Command::DismissServerAlert(ServerAlert::IdlePrimaryDevice));
                                })
                                .child(Icon::new(IconName::X).size_4().text_color(palette.text_secondary)),
                        )
                    })
                    .into_any_element(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use signal_backend::Event;
    use signal_backend::server_alerts::{AlertInfo, ServerAlerts};

    use super::*;
    use crate::views::test_support::{demo, wait};

    #[gpui_kit::test]
    fn the_captcha_dialog_opens_and_its_link_answers(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        wait(&mut cx, "the account", &mut |cx| state.read_with(cx, |s, _| s.account.is_some()));
        cx.update(|window, cx| open_captcha(state.clone(), window, cx));
        wait(&mut cx, "the captcha dialog", &mut |cx| cx.debug_bounds("captcha-dialog").is_some());
        // The browser hands back the captcha; the demo accepts it.
        let id = "signal-hcaptcha.made-up.challenge.made-up";
        state.update(&mut cx, |s, cx| s.open_signal_link(&format!("signalcaptcha://{id}/"), cx));
        assert!(state.read_with(&cx, |s, _| s.captcha_pending));
        wait(&mut cx, "the answer", &mut |cx| state.read_with(cx, |s, _| !s.captcha_pending));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn alerts_and_clock_skew_show_atop_the_chat_list(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let mut alerts = ServerAlerts::new();
        alerts.insert(ServerAlert::IdlePrimaryDevice, AlertInfo { first_received_at: 1, ..Default::default() });
        state.update(&mut cx, |s, cx| s.handle(Event::ServerAlerts(alerts.clone()), cx));
        wait(&mut cx, "the idle warning", &mut |cx| cx.debug_bounds("idle-alert").is_some());
        alerts.insert(ServerAlert::CriticalIdlePrimaryDevice, AlertInfo { first_received_at: 1, ..Default::default() });
        state.update(&mut cx, |s, cx| s.handle(Event::ServerAlerts(alerts), cx));
        wait(&mut cx, "the critical warning", &mut |cx| cx.debug_bounds("critical-idle-alert").is_some());
        // The clock comes first.
        state.update(&mut cx, |s, cx| s.handle(Event::ClockSkew(true), cx));
        wait(&mut cx, "the clock warning", &mut |cx| cx.debug_bounds("clock-skew").is_some());
        backend.shutdown();
    }
}
