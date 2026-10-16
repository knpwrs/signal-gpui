//! Desktop's Privacy page: read receipts and typing indicators, stories,
//! and automatic key verification.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, Window, div};
use signal_backend::AccountSetting;

use super::{Settings, action, list, switch};
use crate::state::AppState;
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let (read_receipts, typing, stories_disabled, kt_disabled) = {
        let s = state.read(cx);
        (s.read_receipts, s.typing_indicators, s.stories_disabled, s.key_transparency_disabled)
    };
    let flag = |id: &'static str, label: &'static str, setting: AccountSetting, on: bool| {
        let state = state.clone();
        switch(id, label, None, on, false, palette, move |on, _, cx| state.update(cx, |s, cx| s.set_account_setting(setting, on, cx)))
    };
    let stories_state = state.clone();
    let stories_button = if stories_disabled {
        Button::new("stories-toggle").outline().label("Turn on stories").on_click(move |_, window, cx| {
            stories_state.update(cx, |s, cx| s.set_stories_disabled(false, cx));
            window.refresh();
        })
    } else {
        Button::new("stories-toggle")
            .danger()
            .label("Turn off stories")
            .on_click(move |_, window, cx| confirm_stories_off(stories_state.clone(), false, window, cx))
    };
    let mut advanced = Vec::new();
    if signal_backend::key_transparency_on() {
        let kt_state = state.clone();
        advanced.push(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(switch(
                    "key-transparency-toggle",
                    "Automatic key verification",
                    Some("When enabled, Signal will attempt to automatically verify the encryption of 1:1 chats.".into()),
                    !kt_disabled,
                    false,
                    palette,
                    move |on, _, cx| kt_state.update(cx, |s, cx| s.set_key_transparency_disabled(!on, cx)),
                ))
                .child(
                    div().child(
                        Button::new("key-transparency-learn-more")
                            .link()
                            .label("Learn More")
                            .on_click(|_, _, cx| cx.open_url("https://support.signal.org/hc/articles/10223569377562")),
                    ),
                )
                .into_any_element(),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(list(
            Some("Messaging"),
            Some("See and share when message are being read and typed. If disabled, you won’t see read receipts or typing indicators from others.".into()),
            [
                flag("read-receipts", "Read receipts", AccountSetting::ReadReceipts, read_receipts),
                flag("typing-indicators", "Typing indicators", AccountSetting::TypingIndicators, typing),
            ],
            palette,
        ))
        .child(list(
            Some("Stories"),
            None,
            [action(
                "Share & View Stories",
                Some("If you opt out of stories you will no longer be able to share or view stories.".into()),
                stories_button,
                palette,
            )],
            palette,
        ))
        .when(!advanced.is_empty(), |d| d.child(list(Some("Advanced"), None, advanced, palette)))
        .into_any_element()
}

/// Desktop's "Turn off stories" confirmation (`Preferences`). It deletes
/// the stories we sent, as its text says; Desktop's own code
/// (`ts/state/smart/Preferences.preload.tsx`) calls `deleteAllMyStories`
/// on turning stories back on instead, which looks like a slip. Story
/// privacy closes with it (`close_all`); Settings stays open.
pub fn confirm_stories_off(state: Entity<AppState>, close_all: bool, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .description("You will no longer be able to share or view stories. Story updates you have recently shared will also be deleted.")
            .confirm()
            .ok_text("Turn off")
            .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.update(cx, |s, cx| s.set_stories_disabled(true, cx));
                if close_all {
                    window.close_all_dialogs(cx);
                }
                true
            })
    });
}
