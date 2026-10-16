//! Desktop's edit history (`ts/components/EditHistoryMessagesModal.dom.tsx`),
//! opened by clicking "Edited" on a message: the message as it is now, then
//! under "Edit history" each earlier version, newest first, every one with
//! its own time and no reactions. Right-clicking one copies its timestamp.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, ClipboardItem, FontWeight, WeakEntity, Window, div, px};
use signal_store::{EditRevision, Message};

use crate::theme::Palette;
use crate::views::common::{day_label, same_day};
use crate::views::conversation::ConversationView;

/// Loads the message's earlier versions and shows them; nothing opens when
/// there are none (Desktop logs "no edit history for message").
pub fn open(view: WeakEntity<ConversationView>, current: Message, window: &mut Window, cx: &mut App) {
    let Some(this) = view.upgrade() else { return };
    let backend = this.read(cx).state.read(cx).backend.clone();
    let db = backend.db().clone();
    let id = current.id;
    let query = backend.spawn(async move { db.edit_history(id).await });
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let history = match query.await {
            Ok(history) => history,
            Err(error) => return tracing::warn!(%error, "couldn't load a message's edit history"),
        };
        if history.is_empty() {
            return;
        }
        let versions = versions(&current, &history);
        let _ = window_handle.update(cx, |_, window, cx| show(view, versions, window, cx));
    })
    .detach();
}

/// Each version as a message to draw, newest (the message as it is) first,
/// as Desktop's `editHistory`: no reactions, no "Edited", and the earlier
/// ones dated by their own timestamps. They get ids of their own (below
/// zero), so revealing a spoiler in one doesn't reveal it in the others.
pub(crate) fn versions(current: &Message, history: &[EditRevision]) -> Vec<Message> {
    let base = Message { reactions: Vec::new(), long_text: None, ..current.clone() };
    let mut versions = vec![base.clone()];
    for (ix, revision) in history.iter().rev().enumerate() {
        versions.push(Message {
            id: -(current.id * 64 + ix as i64 + 1),
            sent_at: revision.sent_at,
            body: revision.body.clone(),
            mentions: revision.mentions.clone(),
            styles: revision.styles.clone(),
            edited: false,
            // The preview was the latest text's; earlier ones' weren't kept.
            link_preview: None,
            // Only text is edited.
            content: None,
            ..base.clone()
        });
    }
    versions
}

/// Whether a date goes above a version: always above the current one, and
/// above an earlier one only when it's later and on another day than the
/// one above it (Desktop's check, which "avoids strange header behavior for
/// out-of-order messages").
pub(crate) fn date_above(previous: Option<i64>, sent_at: i64) -> bool {
    previous.is_none_or(|previous| sent_at > previous && !same_day(previous, sent_at))
}

fn show(view: WeakEntity<ConversationView>, versions: Vec<Message>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let Some(view) = view.upgrade() else { return dialog };
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut previous = None;
        for (ix, version) in versions.iter().enumerate() {
            if ix == 1 {
                rows.push(div().my_3().h(px(1.)).bg(palette.panel_border).into_any_element());
                rows.push(div().pb_2().font_weight(FontWeight::SEMIBOLD).child("Edit history").into_any_element());
            }
            if date_above(previous, version.sent_at) {
                rows.push(
                    div()
                        .py_2()
                        .flex()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.text_secondary)
                        .child(day_label(version.sent_at))
                        .into_any_element(),
                );
            }
            previous = Some(version.sent_at);
            let bubble = view.update(cx, |view, cx| view.render_history_bubble(version, palette, cx));
            let outgoing = version.direction == signal_store::Direction::Outgoing;
            let timestamp = version.sent_at;
            rows.push(
                div()
                    .id(("edit-version", ix))
                    .debug_selector(move || format!("edit-version-{ix}"))
                    .flex()
                    .py_0p5()
                    .when(outgoing, |d| d.justify_end())
                    .child(div().max_w(gpui_kit::relative(0.8)).child(bubble))
                    .context_menu(move |menu, _, _| {
                        menu.item(PopupMenuItem::new("Copy timestamp").on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(timestamp.to_string()))
                        }))
                    })
                    .into_any_element(),
            );
        }
        dialog.title("Edit history").w(px(480.)).child(
            div()
                .id("edit-history")
                .debug_selector(|| "edit-history".into())
                .max_h(px(520.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(rows),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_go_newest_first_without_reactions() {
        let mut current = Message {
            id: 7,
            conversation_id: signal_store::ConversationId("d:a".into()),
            sent_at: 100,
            received_at: 100,
            direction: signal_store::Direction::Outgoing,
            author: None,
            author_name: None,
            body: "meet at 9:30".into(),
            status: signal_store::DeliveryStatus::Sent,
            reactions: vec![signal_store::Reaction { emoji: "👍".into(), author: Some("b".into()), sent_at: 120 }],
            quote: None,
            attachments: vec![],
            link_preview: None,
            deleted: false,
            edited: true,
            expires_in: 0,
            expire_start: None,
            edit_count: 2,
            notice: None,
            mentions: vec![],
            styles: vec![],
            cta: None,
            deleted_by_admin: None,
            long_text: None,
            poll: None,
            view_once: None,
            sealed_sender: vec![],
            content: None,
            is_read: true,
        };
        let history = [
            EditRevision { sent_at: 100, body: "meet at 8".into(), mentions: vec![], styles: vec![] },
            EditRevision { sent_at: 200, body: "meet at 9".into(), mentions: vec![], styles: vec![] },
        ];
        current.sent_at = 100;
        let shown = versions(&current, &history);
        assert_eq!(shown.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["meet at 9:30", "meet at 9", "meet at 8"]);
        assert!(shown.iter().all(|m| m.reactions.is_empty()));
        assert!(shown[0].edited && !shown[1].edited, "only the current one says Edited");
        assert_eq!((shown[1].sent_at, shown[2].sent_at), (200, 100));
        let ids: std::collections::HashSet<i64> = shown.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), 3, "each version has its own id");
        assert!(shown[1..].iter().all(|m| m.id < 0));
    }

    #[test]
    fn dates_show_as_desktop_shows_them() {
        let day = 24 * 60 * 60 * 1000;
        use chrono::TimeZone as _;
        let noon = chrono::Local.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).single().unwrap().timestamp_millis();
        assert!(date_above(None, noon), "above the current version");
        assert!(!date_above(Some(noon), noon - 2 * day), "earlier versions, older: none");
        assert!(date_above(Some(noon - 2 * day), noon), "out of order and another day");
        assert!(!date_above(Some(noon), noon + 1), "the same day");
    }
}
