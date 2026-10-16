//! Desktop's message "Info": when it was sent and received, how far it
//! got, and (with "Show status icon" on) whether it went by sealed
//! sender. (Per-recipient receipts aren't tracked yet; this shows the
//! message's overall status.)

use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, SharedString, Window, div, px};
use signal_store::{DeliveryStatus, Direction, Message};

use crate::state::AppState;
use crate::theme::Palette;

pub fn open(state: Entity<AppState>, message: Message, window: &mut Window, cx: &mut App) {
    let sealed = sealed_sender(state.read(cx), &message);
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
            DeliveryStatus::Failed => "Not sent",
        };
        let disappears = message.expire_start.map(|start| {
            let left = start + i64::from(message.expires_in) * 1000 - chrono::Utc::now().timestamp_millis();
            crate::views::chat_menu::format_timer_long((left.max(0) / 1000) as u32)
        });
        dialog.title("Message details").w(px(420.)).child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .text_sm()
                .child(row("Sent", full_time(message.sent_at)))
                .when(!outgoing, |d| d.child(row("Received", full_time(message.received_at))))
                .when(outgoing, |d| d.child(row("Status", status.into())))
                .when_some(disappears, |d, left| d.child(row("Disappears in", left)))
                .when_some(sealed.clone(), |d, sealed| d.child(row("Sealed sender", sealed))),
        )
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
