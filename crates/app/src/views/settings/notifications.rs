//! Desktop's Notifications page.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, div};

use super::{Choice, Settings, list, select, switch};
use crate::notifications::NotificationSetting;
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let current = state.read(cx).notification_setting;
    let enabled = current != NotificationSetting::Off;
    let toggle_state = state.clone();
    let choice = |label: &'static str, setting: NotificationSetting| {
        let state = state.clone();
        Choice::new(label, current == setting, move |_, cx| state.update(cx, |s, cx| s.set_notification_setting(setting, cx)))
    };
    // While off, Desktop's select falls back to the first choice.
    let shown = |setting: NotificationSetting| if enabled { setting } else { NotificationSetting::NameAndMessage };
    let show = vec![
        Choice { chosen: shown(current) == NotificationSetting::NameAndMessage, ..choice("Name, content, and actions", NotificationSetting::NameAndMessage) },
        Choice { chosen: shown(current) == NotificationSetting::NameOnly, ..choice("Name only", NotificationSetting::NameOnly) },
        Choice { chosen: shown(current) == NotificationSetting::NoNameOrMessage, ..choice("No name or content", NotificationSetting::NoNameOrMessage) },
    ];
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
            ],
            palette,
        ))
        .into_any_element()
}
