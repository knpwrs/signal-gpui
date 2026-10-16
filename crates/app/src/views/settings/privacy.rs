//! Desktop's Privacy page (phone number privacy, blocked people and groups,
//! read receipts and typing indicators, the default disappearing timer,
//! stories, automatic key verification) and its Phone Number and Blocked
//! pages.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, SharedString, TestSupportExt as _, Window, div, px};
use signal_backend::AccountSetting;

use super::{Choice, Page, Settings, action, go, link, list, select, switch};
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
    let (blocked, timer) = {
        let s = state.read(cx);
        let blocked = s.blocked();
        let groups = blocked.iter().filter(|c| c.kind == signal_store::ConversationKind::Group).count();
        (blocked_count(blocked.len() - groups, groups), s.prefs.universal_timer)
    };
    let (pnp_settings, blocked_settings) = (settings.clone(), settings.clone());
    let none_blocked = blocked.1;
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
            None,
            None,
            [link(
                "phone-number-privacy",
                "Phone Number",
                Some("Choose who can see your phone number and who can contact you on Signal with it.".into()),
                None,
                false,
                palette,
                move |window, cx| go(&pnp_settings, Page::PhoneNumber, window, cx),
            )],
            palette,
        ))
        .child(list(
            None,
            None,
            [link("blocked", "Blocked", Some(blocked.0), None, none_blocked, palette, move |window, cx| {
                go(&blocked_settings, Page::Blocked, window, cx)
            })],
            palette,
        ))
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
            Some("Disappearing messages"),
            None,
            [timer_select(&state, timer, palette)],
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

/// Desktop's `Preferences--blocked-count-…-new`, and whether there are none.
fn blocked_count(people: usize, groups: usize) -> (SharedString, bool) {
    let plural = |n: usize, one: &str, other: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {other}") };
    let text = match (people, groups) {
        (0, 0) => "No users or groups".into(),
        (p, 0) => plural(p, "user", "users"),
        (0, g) => plural(g, "group", "groups"),
        (p, g) => plural(p + g, "user or group", "users or groups"),
    };
    (text.into(), people + groups == 0)
}

/// Desktop's "Default timer for new chats": its choices, then "Custom
/// time" (or the custom time set).
fn timer_select(state: &Entity<AppState>, current: u32, palette: Palette) -> AnyElement {
    use crate::views::chat_menu::{TIMER_CHOICES, format_timer_long};
    let mut choices: Vec<Choice> = TIMER_CHOICES
        .into_iter()
        .map(|seconds| {
            let state = state.clone();
            Choice::new(format_timer_long(seconds), seconds == current, move |_, cx| {
                state.update(cx, |s, cx| s.set_universal_timer(seconds, cx))
            })
        })
        .collect();
    let custom = !TIMER_CHOICES.contains(&current);
    let state = state.clone();
    choices.push(Choice::new(
        if custom { format_timer_long(current) } else { "Custom time".into() },
        custom,
        move |window, cx| {
            let state = state.clone();
            crate::views::chat_menu::custom_time(
                current,
                move |seconds, _, cx| state.update(cx, |s, cx| s.set_universal_timer(seconds, cx)),
                window,
                cx,
            );
        },
    ));
    select(
        "universal-timer",
        "Default timer for new chats",
        Some("Set a default disappearing message timer for all new chats started by you.".into()),
        choices,
        false,
        palette,
    )
}

/// A choice of one (Desktop's `SettingsRadio`).
fn radio(id: &'static str, label: &'static str, chosen: bool, palette: Palette, on_click: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            on_click(window, cx);
            window.refresh();
        })
        .child(
            div()
                .size(px(16.))
                .flex_none()
                .rounded_full()
                .border_2()
                .border_color(if chosen { palette.accent } else { palette.text_secondary })
                .when(chosen, |d| d.bg(palette.accent)),
        )
        .child(div().text_sm().child(label))
        .test_support()
        .into_any_element()
}

/// Desktop's Phone Number page: who can see our number, and who can find
/// us by it.
pub(super) fn render_phone_number(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let (shared, discoverable) = {
        let prefs = &state.read(cx).prefs;
        (prefs.number_shared, prefs.number_discoverable)
    };
    let sharing = if shared {
        "Your phone number will be visible to people and groups you message."
    } else if discoverable {
        "Your phone number will not be visible to anyone unless they have it saved in their phone’s contacts."
    } else {
        "Your phone number will not be visible to anyone."
    };
    let finding = if discoverable {
        "Anyone who has your phone number will see you're on Signal and can start chats with you."
    } else {
        "Nobody will be able to see you're on Signal unless you message them or have an existing chat with them."
    };
    let see = |id: &'static str, label: &'static str, everybody: bool| {
        let state = state.clone();
        radio(id, label, shared == everybody, palette, move |_, cx| state.update(cx, |s, cx| s.set_number_shared(everybody, cx)))
    };
    let find_everyone = {
        let state = state.clone();
        radio("pnp-find-everyone", "Everyone", discoverable, palette, move |_, cx| {
            state.update(cx, |s, cx| s.set_number_discoverable(true, cx))
        })
    };
    let find_nobody = {
        let state = state.clone();
        radio("pnp-find-nobody", "Nobody", !discoverable, palette, move |window, cx| {
            if state.read(cx).prefs.number_shared {
                // Desktop's `WhoCanFindMeReadOnlyToast`.
                use gpui_kit::component::notification::Notification;
                window.push_notification(Notification::new().message("To change this setting, set “Who can see my number” to “Nobody”."), cx);
            } else if state.read(cx).prefs.number_discoverable {
                confirm_not_discoverable(state.clone(), window, cx);
            }
        })
    };
    let section = |title: &'static str, rows: Vec<AnyElement>, description: &'static str| {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
            .children(rows)
            .child(div().text_xs().text_color(palette.text_secondary).child(description))
    };
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(section(
            "Who can see my number",
            vec![see("pnp-see-everyone", "Everyone", true), see("pnp-see-nobody", "Nobody", false)],
            sharing,
        ))
        .child(section("Who can find me by number", vec![find_everyone, find_nobody], finding))
        .into_any_element()
}

/// Desktop's confirmation before nobody can find us by number.
fn confirm_not_discoverable(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .title("Are you sure?")
            .description(
                "If you change “Who can find me by number” to “Nobody” it will make it harder for people to find you on Signal.",
            )
            .confirm()
            .ok_text("OK")
            .on_ok(move |_, window, cx| {
                state.update(cx, |s, cx| s.set_number_discoverable(false, cx));
                window.refresh();
                true
            })
    });
}

/// Desktop's Blocked page: blocked people, then groups, each with Unblock
/// (Desktop lists them without one; unblocking is in the chat there).
pub(super) fn render_blocked(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    use crate::views::common::conversation_avatar;
    let state = settings.read(cx).state.clone();
    let blocked: Vec<signal_store::Conversation> = state.read(cx).blocked().into_iter().cloned().collect();
    let row = |ix: usize, chat: &signal_store::Conversation| {
        let (state, id) = (state.clone(), chat.id.clone());
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(conversation_avatar(chat, px(36.)))
            .child(div().flex_1().min_w_0().truncate().text_sm().child(chat.title.clone()))
            .child(Button::new(("unblock", ix)).outline().label("Unblock").on_click(move |_, window, cx| {
                state.read(cx).set_chat(id.clone(), signal_backend::ChatChange::Blocked(false));
                window.refresh();
            }))
            .into_any_element()
    };
    let (groups, people): (Vec<_>, Vec<_>) =
        blocked.iter().enumerate().partition(|(_, c)| c.kind == signal_store::ConversationKind::Group);
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(div().text_sm().text_color(palette.text_secondary).child("Blocked users will not be able to call you or send you messages"))
        .when(!people.is_empty(), |d| d.child(list(Some("Blocked Users"), None, people.iter().map(|(ix, c)| row(*ix, c)), palette)))
        .when(!groups.is_empty(), |d| d.child(list(Some("Blocked Groups"), None, groups.iter().map(|(ix, c)| row(*ix, c)), palette)))
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
