//! A chat's menu: Signal Desktop's conversation header "⋯" menu, also used
//! as the chat list's right-click menu. Labels and order follow Desktop.

use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{App, Context, Entity, Window};
use signal_backend::{ChatChange, Command, MUTED_ALWAYS};
use signal_store::{Conversation, ConversationId, ConversationKind};

use crate::state::AppState;

const HOUR_MS: i64 = 60 * 60 * 1000;
/// Desktop's mute choices ("Until…" with a date picker isn't here yet).
const MUTE_FOR: [(&str, i64); 4] =
    [("1 hour", HOUR_MS), ("8 hours", 8 * HOUR_MS), ("1 day", 24 * HOUR_MS), ("1 week", 7 * 24 * HOUR_MS)];

/// Desktop's timer choices (`DEFAULT_DURATIONS_IN_SECONDS`).
const TIMER_CHOICES: [u32; 8] = [0, 4 * WEEK, WEEK, DAY, 8 * HOUR, HOUR, 5 * MINUTE, 30];
const MINUTE: u32 = 60;
const HOUR: u32 = 60 * MINUTE;
const DAY: u32 = 24 * HOUR;
const WEEK: u32 = 7 * DAY;

/// "Off", "4 weeks", "1 day", "30 seconds": the largest whole unit, as
/// Desktop labels its timer choices.
pub fn format_timer_long(seconds: u32) -> String {
    if seconds == 0 {
        return "Off".into();
    }
    let (n, unit) = UNITS.iter().rev().map(|(size, unit)| (seconds / size, *unit)).find(|(n, _)| *n > 0).unwrap_or((seconds, "second"));
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

const UNITS: [(u32, &str); 5] = [(1, "second"), (MINUTE, "minute"), (HOUR, "hour"), (DAY, "day"), (WEEK, "week")];
/// Desktop's custom-time ranges per unit (the upper bound is exclusive).
const UNIT_RANGES: [(u32, u32); 5] = [(1, 60), (1, 60), (1, 24), (1, 7), (1, 5)];

/// The custom time being picked: a count of one unit.
struct CustomTime {
    value: u32,
    unit: usize,
}

impl CustomTime {
    /// Desktop starts from the current timer in its largest whole unit.
    fn from_seconds(seconds: u32) -> Self {
        let seconds = seconds.max(1);
        let unit = UNITS.iter().rposition(|(size, _)| seconds >= *size).unwrap_or(0);
        let (min, max) = UNIT_RANGES[unit];
        Self { value: (seconds / UNITS[unit].0).clamp(min, max - 1), unit }
    }

    fn seconds(&self) -> u32 {
        self.value * UNITS[self.unit].0
    }

    fn set_unit(&mut self, unit: usize) {
        let (min, max) = UNIT_RANGES[unit];
        self.unit = unit;
        self.value = self.value.clamp(min, max - 1);
    }

    fn step(&mut self, by: i32) {
        let (min, max) = UNIT_RANGES[self.unit];
        self.value = (self.value as i32 + by).clamp(min as i32, max as i32 - 1) as u32;
    }
}

/// Desktop's "Custom Time" dialog.
fn custom_timer(state: Entity<AppState>, conversation: ConversationId, current: u32, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    use gpui_kit::prelude::*;
    use gpui_kit::{div, px};
    let pick = cx.new(|_| CustomTime::from_seconds(current));
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = crate::theme::Palette::get(cx);
        let time = pick.read(cx);
        let (value, unit) = (time.value, time.unit);
        let stepper = |key: &'static str, label: &'static str, by: i32| {
            let pick = pick.clone();
            Button::new(key).label(label).outline().on_click(move |_, window, cx| {
                pick.update(cx, |p, _| p.step(by));
                window.refresh();
            })
        };
        let (state, conversation, set) = (state.clone(), conversation.clone(), pick.clone());
        dialog
            .title("Custom Time")
            .w(px(420.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(div().text_sm().text_color(palette.text_secondary).child("Choose a custom time for disappearing messages."))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(stepper("custom-time-less", "−", -1))
                            .child(div().w(px(36.)).text_center().child(value.to_string()))
                            .child(stepper("custom-time-more", "+", 1)),
                    )
                    .child(div().flex().flex_wrap().gap_1().children(["Seconds", "Minutes", "Hours", "Days", "Weeks"].iter().enumerate().map(
                        |(ix, label)| {
                            let pick = pick.clone();
                            Button::new(("custom-time-unit", ix))
                                .label(*label)
                                .when(ix == unit, |b| b.primary())
                                .when(ix != unit, |b| b.outline())
                                .on_click(move |_, window, cx| {
                                    pick.update(cx, |p, _| p.set_unit(ix));
                                    window.refresh();
                                })
                        },
                    ))),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("custom-time-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("custom-time-set").label("Set").primary().on_click(move |_, window, cx| {
                        let seconds = set.read(cx).seconds();
                        state.read(cx).send(Command::SetExpireTimer { conversation: conversation.clone(), seconds });
                        window.close_all_dialogs(cx);
                    })),
            )
    });
}

/// What the conversation header adds to the menu (the chat list's
/// right-click menu has none of these).
#[derive(Clone, Default)]
pub struct HeaderActions {
    pub select_messages: Option<MenuAction>,
}

/// A menu item's handler that reaches back into the view that built it.
pub type MenuAction = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

pub fn build(
    menu: PopupMenu,
    conversation: &Conversation,
    state: &Entity<AppState>,
    header: HeaderActions,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let id = conversation.id.clone();
    let note_to_self = conversation.kind == ConversationKind::NoteToSelf;
    let group = conversation.kind == ConversationKind::Group;
    let change = |change: ChatChange| {
        let (state, id) = (state.clone(), id.clone());
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| state.read(cx).set_chat(id.clone(), change)
    };

    // The Signal chat (release notes) has no timer or settings, as Desktop's.
    let signal = conversation.id.is_signal();
    // Groups: only people allowed to edit the group's details.
    // Desktop's `canChangeTimer`: not in an ended group either.
    let timer_locked = signal || conversation.blocked || conversation.left || conversation.ended || (group && !conversation.can_edit_info);
    let menu = if timer_locked {
        menu
    } else {
        let (state, id, current) = (state.clone(), id.clone(), conversation.expire_timer);
        menu.submenu_with_icon(Some(IconName::Timer.into()), "Disappearing messages", window, cx, move |mut menu, _, _| {
            for seconds in TIMER_CHOICES {
                let (state, id) = (state.clone(), id.clone());
                menu = menu.item(
                    PopupMenuItem::new(format_timer_long(seconds))
                        .checked(current == seconds)
                        .on_click(move |_, _, cx| state.read(cx).send(Command::SetExpireTimer { conversation: id.clone(), seconds })),
                );
            }
            let (state, id) = (state.clone(), id.clone());
            menu.item(
                PopupMenuItem::new("Custom time...")
                    .checked(!TIMER_CHOICES.contains(&current))
                    .on_click(move |_, window, cx| custom_timer(state.clone(), id.clone(), current, window, cx)),
            )
        })
    };

    let menu = if conversation.muted {
        menu.item(PopupMenuItem::new("Unmute").icon(IconName::Bell).on_click(change(ChatChange::MutedUntil(0))))
    } else {
        let (state, id) = (state.clone(), id.clone());
        menu.submenu_with_icon(Some(IconName::BellOff.into()), "Mute notifications", window, cx, move |menu, _, _| {
            let mut menu = menu.label("Mute this chat for…");
            for (label, ms) in MUTE_FOR {
                let (state, id) = (state.clone(), id.clone());
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    let until = chrono::Utc::now().timestamp_millis() + ms;
                    state.read(cx).set_chat(id.clone(), ChatChange::MutedUntil(until));
                }));
            }
            let (state, id) = (state.clone(), id.clone());
            menu.item(
                PopupMenuItem::new("Always")
                    .on_click(move |_, _, cx| state.read(cx).set_chat(id.clone(), ChatChange::MutedUntil(MUTED_ALWAYS))),
            )
        })
    };

    let mut menu = menu;
    if !note_to_self && !signal {
        let (state, chat) = (state.clone(), conversation.clone());
        menu = menu.item(
            PopupMenuItem::new(if group { "Group settings" } else { "Chat settings" })
                .icon(IconName::Settings)
                .on_click(move |_, window, cx| crate::views::chat_settings::open(state.clone(), chat.clone(), window, cx)),
        );
    }
    // The header's menu also has "All media".
    if header.select_messages.is_some() {
        let (state, id) = (state.clone(), id.clone());
        menu = menu.item(
            PopupMenuItem::new("All media")
                .icon(IconName::Album)
                .on_click(move |_, window, cx| crate::views::all_media::open(state.clone(), id.clone(), window, cx)),
        );
    }
    let mut menu = menu.separator();
    if let Some(select) = header.select_messages {
        menu = menu
            .item(PopupMenuItem::new("Select messages").icon(IconName::SquareCheck).on_click(move |_, window, cx| select(window, cx)))
            .separator();
    }
    if !conversation.marked_unread {
        menu = menu
            .item(PopupMenuItem::new("Mark as unread").icon(IconName::MessageSquareDot).on_click(change(ChatChange::MarkedUnread(true))));
    }
    // Pinning an archived chat would hide it; unarchive it first.
    if conversation.pinned {
        let (state, id) = (state.clone(), id.clone());
        menu = menu.item(
            PopupMenuItem::new("Unpin chat").icon(IconName::PinOff).on_click(move |_, _, cx| state.read(cx).set_pinned(id.clone(), false)),
        );
    } else if !conversation.archived {
        let full = state.read(cx).pinned_count() >= signal_backend::remote_config().pinned_chat_limit();
        let (state, id) = (state.clone(), id.clone());
        menu = menu.item(
            PopupMenuItem::new("Pin chat")
                .icon(IconName::Pin)
                .disabled(full)
                .on_click(move |_, _, cx| state.read(cx).set_pinned(id.clone(), true)),
        );
    }
    menu = if conversation.archived {
        menu.item(PopupMenuItem::new("Unarchive").icon(IconName::ArchiveRestore).on_click(change(ChatChange::Archived(false))))
    } else {
        menu.item(PopupMenuItem::new("Archive").icon(IconName::Archive).on_click(change(ChatChange::Archived(true))))
    };
    if !note_to_self {
        let (state, id, title, blocked) = (state.clone(), id.clone(), conversation.title.clone(), conversation.blocked);
        menu = menu.item(PopupMenuItem::new(if blocked { "Unblock" } else { "Block" }).icon(IconName::Ban).on_click(
            move |_, window, cx| confirm_block(state.clone(), id.clone(), &title, group, !blocked, window, cx),
        ));
    }
    let (delete_state, delete_id) = (state.clone(), id.clone());
    menu = menu.item(
        PopupMenuItem::new("Delete")
            .icon(IconName::Trash)
            .on_click(move |_, window, cx| confirm_delete_chat(delete_state.clone(), delete_id.clone(), group, window, cx)),
    );
    // Desktop doesn't offer leaving an ended group.
    if group && !conversation.left && !conversation.ended {
        let (state, id) = (state.clone(), id.clone());
        menu = menu.item(
            PopupMenuItem::new("Leave group")
                .icon(IconName::LogOut)
                .on_click(move |_, window, cx| crate::views::chat_settings::confirm_leave(state.clone(), id.clone(), window, cx)),
        );
    }
    menu
}

/// Desktop's block and unblock confirmations.
pub fn confirm_block(
    state: Entity<AppState>,
    conversation: ConversationId,
    title: &str,
    group: bool,
    block: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let (heading, body, ok) = match (block, group) {
        (true, false) => (format!("Block {title}?"), "Blocked people won't be able to call you or send you messages.", "Block"),
        (true, true) => (
            format!("Block {title}?"),
            "You will no longer receive messages or updates from this group and members won't be able to add you to this group again.",
            "Block",
        ),
        (false, false) => (format!("Unblock {title}?"), "You will be able to message and call each other.", "Unblock"),
        (false, true) => (format!("Unblock {title}?"), "Group members will be able to add you to this group again.", "Unblock"),
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, conversation) = (state.clone(), conversation.clone());
        alert
            .title(heading.clone())
            .description(body)
            .confirm()
            .ok_text(ok)
            .ok_variant(if block { ButtonVariant::Danger } else { ButtonVariant::Primary })
            .on_ok(move |_, window, cx| {
                state.read(cx).set_chat(conversation.clone(), ChatChange::Blocked(block));
                window.close_all_dialogs(cx);
                true
            })
    });
}

/// Desktop's "Delete chat?": gone here and from your other devices.
pub fn confirm_delete_chat(state: Entity<AppState>, conversation: ConversationId, group: bool, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, conversation) = (state.clone(), conversation.clone());
        alert
            .title("Delete chat?")
            .description(if group {
                "All messages in this chat will be deleted from all your devices. You will still be a member of this group."
            } else {
                "All messages in this chat will be deleted from all your devices."
            })
            .confirm()
            .ok_text("Delete")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.read(cx).send(Command::DeleteConversation { conversation: conversation.clone() });
                window.close_all_dialogs(cx);
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_labels_match_desktop() {
        let labels: Vec<String> = TIMER_CHOICES.iter().map(|s| format_timer_long(*s)).collect();
        assert_eq!(labels, ["Off", "4 weeks", "1 week", "1 day", "8 hours", "1 hour", "5 minutes", "30 seconds"]);
    }

    #[test]
    fn custom_time_starts_from_the_current_timer_and_stays_in_range() {
        let mut t = CustomTime::from_seconds(3 * DAY);
        assert_eq!((t.value, t.unit, t.seconds()), (3, 3, 3 * DAY));
        t.step(10);
        assert_eq!(t.value, 6, "days go up to 6");
        t.set_unit(4);
        assert_eq!((t.value, t.seconds()), (4, 4 * WEEK), "clamped to the weeks range");
        assert_eq!(CustomTime::from_seconds(0).seconds(), 1);
    }
}
