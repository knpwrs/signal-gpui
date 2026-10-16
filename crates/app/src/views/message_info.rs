//! Desktop's message details (`MessageDetail.dom.tsx`): when it was sent
//! and received, and for ours, its recipients grouped by how far it got
//! with each (Failed, Viewed, Read, Delivered, Sent, Pending), each with
//! their photo, when that last changed, the sealed sender icon (with "Show
//! status icon" on), and "View safety number" when their key changed.
//! Failed sends can be sent again from here ("Retry Send").

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, SharedString, Window, div, px};
use signal_store::{ConversationId, DeliveryStatus, Direction, Message, SendState, SendStatus};

use crate::state::AppState;
use crate::theme::Palette;

/// Desktop's groups, in its order, with their headers
/// (`icu:MessageDetailsHeader--…`).
const GROUPS: [(SendStatus, &str); 6] = [
    (SendStatus::Failed, "Not sent"),
    (SendStatus::Viewed, "Viewed by"),
    (SendStatus::Read, "Read by"),
    (SendStatus::Delivered, "Delivered to"),
    (SendStatus::Sent, "Sent to"),
    (SendStatus::Pending, "Pending"),
];

/// Who each group lists, in Desktop's order (skipped ones aren't shown,
/// as Desktop's `VisibleSendStatus` leaves them out); names sorted.
pub(crate) fn grouped(states: &[SendState], name: impl Fn(&str) -> String) -> Vec<(SendStatus, &'static str, Vec<SendState>)> {
    GROUPS
        .iter()
        .filter_map(|(status, header)| {
            let mut people: Vec<SendState> = states.iter().filter(|s| s.status == *status).cloned().collect();
            people.sort_by_key(|s| name(&s.recipient).to_lowercase());
            (!people.is_empty()).then_some((*status, *header, people))
        })
        .collect()
}

/// Opens the details once the recipients' states are read.
pub fn open(state: Entity<AppState>, message: Message, window: &mut Window, cx: &mut App) {
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let id = message.id;
    let read = backend.spawn(async move { db.send_states(id).await.unwrap_or_default() });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let states = read.await;
        let _ = cx.update_window(handle, |_, window, cx| show(state, message, states, window, cx));
    })
    .detach();
}

fn show(state: Entity<AppState>, message: Message, states: Vec<SendState>, window: &mut Window, cx: &mut App) {
    let sealed = sealed_sender(state.read(cx), &message);
    let indicators = state.read(cx).prefs.flag(signal_backend::AccountSetting::SealedSenderIndicators);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let row = |label: &'static str, value: String| {
            div()
                .flex()
                .gap_4()
                .child(div().w(px(110.)).text_color(palette.text_secondary).child(label))
                .child(div().flex_1().child(SharedString::from(value)))
        };
        let outgoing = message.direction == Direction::Outgoing;
        let status = match message.status {
            DeliveryStatus::Sending => "Pending",
            DeliveryStatus::Sent => "Sent",
            DeliveryStatus::Delivered => "Delivered",
            DeliveryStatus::Read => "Read",
            DeliveryStatus::Viewed => "Viewed",
            DeliveryStatus::PartialSent => "Partially sent",
            DeliveryStatus::Paused => "Send paused",
            DeliveryStatus::Failed => "Not sent",
        };
        let disappears = message.expire_start.map(|start| {
            let left = start + i64::from(message.expires_in) * 1000 - chrono::Utc::now().timestamp_millis();
            crate::views::chat_menu::format_timer_long((left.max(0) / 1000) as u32)
        });
        let app = state.read(cx);
        let name = |sid: &str| {
            app.conversation(&ConversationId::direct(sid)).map_or_else(|| "Unknown contact".to_owned(), |c| c.title.clone())
        };
        let groups = grouped(&states, name);
        let any_failed = groups.iter().any(|(s, ..)| *s == SendStatus::Failed);
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .text_sm()
            .child(row("Sent", full_time(message.sent_at)))
            .when(!outgoing, |d| d.child(row("Received", full_time(message.received_at))))
            // Without recipients' states (older messages, Note to Self),
            // the message's own.
            .when(outgoing && states.is_empty(), |d| d.child(row("Status", status.into())))
            .when_some(disappears, |d, left| d.child(row("Disappears in", left)))
            .when_some(sealed.clone().filter(|_| states.is_empty()), |d, sealed| d.child(row("Sealed sender", sealed)));
        for (status, header, people) in groups {
            body = body.child(
                div()
                    .debug_selector(move || format!("details-{header}"))
                    .pt_3()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(header),
            );
            for person in people {
                let who = name(&person.recipient);
                let photo = app.photo_of(&person.recipient).map(|p| p.to_path_buf());
                let color = crate::theme::color_for(&person.recipient);
                let sealed = indicators && message.sealed_sender.contains(&person.recipient);
                let identity = status == SendStatus::Failed && person.error.as_deref() == Some(signal_store::SEND_ERROR_IDENTITY);
                let error = person.error.clone().filter(|_| status == SendStatus::Failed && !identity);
                let (safety_state, chat, safety_name) = (state.clone(), ConversationId::direct(&person.recipient), who.clone());
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(crate::views::common::person_avatar(photo.as_deref(), &who, color, px(32.)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().truncate().child(who.clone()))
                                .when_some(error, |d, error| d.child(div().text_xs().text_color(palette.danger).child(error))),
                        )
                        .when(sealed, |d| {
                            d.child(Icon::new(IconName::Lock).size_3p5().text_color(palette.text_secondary))
                        })
                        .when_some(person.updated_at, |d, at| {
                            d.child(div().text_xs().text_color(palette.text_secondary).child(full_time(at)))
                        })
                        .when(identity, |d| {
                            use gpui_kit::component::button::{Button, ButtonVariants as _};
                            d.child(
                                Button::new(SharedString::from(format!("details-safety-{}", person.recipient)))
                                    .label("View safety number")
                                    .ghost()
                                    .on_click(move |_, window, cx| {
                                        crate::views::safety_number::open(safety_state.clone(), chat.clone(), safety_name.clone(), window, cx)
                                    }),
                            )
                        }),
                );
            }
        }
        let retry_state = state.clone();
        let id = message.id;
        dialog.title("Message details").w(px(460.)).child(body).when(any_failed, |dialog| {
            use gpui_kit::component::button::{Button, ButtonVariants as _};
            dialog.footer(gpui_kit::component::dialog::DialogFooter::new().child(
                Button::new("details-retry").label("Retry Send").primary().on_click(move |_, window, cx| {
                    retry_state.read(cx).send(signal_backend::Command::RetrySend { message_id: id });
                    window.close_all_dialogs(cx);
                }),
            ))
        })
    });
}

/// Desktop's sealed sender icon in message details
/// (`isUnidentifiedDelivery`), shown with "Show status icon" on: theirs
/// came by sealed sender, or ours went that way to these people. (Ours
/// sent from this computer aren't known to have.)
pub(crate) fn sealed_sender(state: &AppState, message: &Message) -> Option<String> {
    if !state.prefs.flag(signal_backend::AccountSetting::SealedSenderIndicators) || message.sealed_sender.is_empty() {
        return None;
    }
    if message.direction == Direction::Incoming {
        return Some("Received by sealed sender".into());
    }
    let names: Vec<String> = message
        .sealed_sender
        .iter()
        .map(|sid| {
            state.conversation(&signal_store::ConversationId::direct(sid)).map_or_else(|| "Unknown contact".into(), |c| c.title.clone())
        })
        .collect();
    Some(format!("Sent by sealed sender to {}", names.join(", ")))
}

/// "Monday, March 3, 2025 at 3:41 PM"
fn full_time(millis: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|t| t.format("%A, %B %-d, %Y at %-I:%M %p").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(recipient: &str, status: SendStatus) -> SendState {
        SendState { recipient: recipient.into(), status, updated_at: None, error: None }
    }

    #[test]
    fn recipients_group_in_desktops_order() {
        let states = [
            state("c", SendStatus::Sent),
            state("a", SendStatus::Read),
            state("b", SendStatus::Failed),
            state("d", SendStatus::Read),
            state("e", SendStatus::Skipped),
            state("f", SendStatus::Pending),
        ];
        let groups = grouped(&states, |sid| sid.to_uppercase());
        let shape: Vec<(&str, Vec<&str>)> =
            groups.iter().map(|(_, header, people)| (*header, people.iter().map(|p| p.recipient.as_str()).collect())).collect();
        assert_eq!(shape, [("Not sent", vec!["b"]), ("Read by", vec!["a", "d"]), ("Sent to", vec!["c"]), ("Pending", vec!["f"])]);
    }
}
