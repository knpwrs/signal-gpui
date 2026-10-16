//! Desktop's Notifications page and its "While muted" page. (Desktop's
//! badge count settings, notification profiles and "Reset" aren't here.)

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, div};
use signal_backend::AccountSetting;

use super::{Choice, Page, Settings, go, link, list, select, switch};
use crate::notifications::NotificationSetting;
use crate::prefs::{Prefs, Toggle};
use crate::theme::Palette;

/// Desktop's `getNotifyWhileMutedSummary`.
pub(crate) fn while_muted_summary(prefs: &Prefs) -> &'static str {
    let calls = prefs.flag(AccountSetting::NotifyCallsIfMuted);
    let mentions = prefs.flag(AccountSetting::NotifyMentionsIfMuted);
    let replies = prefs.flag(AccountSetting::NotifyRepliesIfMuted);
    match (calls, mentions, replies) {
        (true, true, true) => "Calls, mentions, replies",
        (true, true, false) => "Calls, mentions",
        (true, false, true) => "Calls, replies",
        (false, true, true) => "Mentions, replies",
        (true, false, false) => "Calls",
        (false, true, false) => "Mentions",
        (false, false, true) => "Replies",
        (false, false, false) => "None",
    }
}

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let (current, prefs) = {
        let s = state.read(cx);
        (s.notification_setting, s.prefs.clone())
    };
    let enabled = current != NotificationSetting::Off;
    let toggle_state = state.clone();
    // While off, Desktop's select falls back to the first choice.
    let shown = if enabled { current } else { NotificationSetting::NameAndMessage };
    let choice = |label: &'static str, setting: NotificationSetting| {
        let state = state.clone();
        Choice::new(label, shown == setting, move |_, cx| state.update(cx, |s, cx| s.set_notification_setting(setting, cx)))
    };
    let show = vec![
        choice("Name, content, and actions", NotificationSetting::NameAndMessage),
        choice("Name only", NotificationSetting::NameOnly),
        choice("No name or content", NotificationSetting::NoNameOrMessage),
    ];
    let flag = |id: &'static str, label: &'static str, description: &'static str, setting: AccountSetting, disabled: bool| {
        let state = state.clone();
        switch(id, label, Some(description.into()), prefs.flag(setting), disabled, palette, move |on, _, cx| {
            state.update(cx, |s, cx| s.set_account_setting(setting, on, cx))
        })
    };
    let toggle = |id: &'static str, label: &'static str, description: Option<&'static str>, toggle: Toggle, disabled: bool| {
        let state = state.clone();
        switch(id, label, description.map(Into::into), prefs.on(toggle), disabled, palette, move |on, _, cx| {
            state.update(cx, |s, cx| s.set_toggle(toggle, on, cx))
        })
    };
    let muted_settings = settings.clone();
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(list(
            None,
            None,
            [
                switch("notifications-enabled", "Enable notifications", None, enabled, false, palette, move |on, _, cx| {
                    let setting = if on { NotificationSetting::NameAndMessage } else { NotificationSetting::Off };
                    toggle_state.update(cx, |s, cx| s.set_notification_setting(setting, cx));
                }),
                select("notification-content", "Show", None, show, !enabled, palette),
                link(
                    "while-muted",
                    "While muted",
                    Some("Choose notifications to show for muted chats.".into()),
                    Some(while_muted_summary(&prefs).into()),
                    !enabled,
                    palette,
                    move |window, cx| go(&muted_settings, Page::WhileMuted, window, cx),
                ),
            ],
            palette,
        ))
        .child(list(
            None,
            None,
            [
                flag(
                    "reaction-notifications",
                    "Reaction notifications",
                    "Notify when someone reacts to your message",
                    AccountSetting::ReactionNotifications,
                    !enabled,
                ),
                flag(
                    "unread-reminders",
                    "Unread reminders",
                    "Occasionally notify when there are unread messages in muted chats.",
                    AccountSetting::ShowUnreadReminders,
                    !enabled,
                ),
            ],
            palette,
        ))
        .child(list(
            Some("Sounds"),
            None,
            [
                toggle("notification-sounds", "Push notification sounds", None, Toggle::NotificationSounds, !enabled),
                toggle(
                    "message-sounds",
                    "In-chat message sounds",
                    Some("Hear a notification sound for sent and received messages while in the chat."),
                    Toggle::MessageSounds,
                    false,
                ),
            ],
            palette,
        ))
        .into_any_element()
}

/// Desktop's "While muted" page.
pub(super) fn render_while_muted(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let prefs = state.read(cx).prefs.clone();
    let flag = |id: &'static str, label: &'static str, description: &'static str, setting: AccountSetting| {
        let state = state.clone();
        switch(id, label, Some(description.into()), prefs.flag(setting), false, palette, move |on, _, cx| {
            state.update(cx, |s, cx| s.set_account_setting(setting, on, cx))
        })
    };
    list(
        None,
        None,
        [
            flag("muted-calls", "Calls", "Ring or notify when a call is started in muted chats.", AccountSetting::NotifyCallsIfMuted),
            flag("muted-mentions", "Mentions", "Notify when you are mentioned in muted chats.", AccountSetting::NotifyMentionsIfMuted),
            flag(
                "muted-replies",
                "Replies",
                "Notify when someone replies to your message in muted chats.",
                AccountSetting::NotifyRepliesIfMuted,
            ),
        ],
        palette,
    )
}
