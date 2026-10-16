//! When a sent message can be edited, following Desktop
//! (`ts/util/canEditMessage.dom.ts`): your own text messages, for a day (any
//! time in Note to Self), up to ten times.

use signal_store::{Direction, Message};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Desktop's `MESSAGE_MAX_EDIT_COUNT`.
pub const MAX_EDITS: u32 = 10;

pub fn can_edit(message: &Message, note_to_self: bool, now: i64) -> bool {
    message.direction == Direction::Outgoing
        && !message.deleted
        && !message.body.is_empty()
        // A poll's body is its question; polls can't be edited.
        && message.poll.is_none()
        // Nor view-once messages (Desktop's `handleEditMessage`).
        && message.view_once.is_none()
        && (note_to_self || (now - message.sent_at).abs() <= DAY_MS)
        && (note_to_self || message.edit_count < MAX_EDITS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use signal_store::{ConversationId, DeliveryStatus};

    fn ours(sent_at: i64, body: &str) -> Message {
        Message {
            id: 1,
            conversation_id: ConversationId::direct("a"),
            sent_at,
            received_at: sent_at,
            direction: Direction::Outgoing,
            author: None,
            author_name: None,
            body: body.into(),
            status: DeliveryStatus::Delivered,
            reactions: Vec::new(),
            quote: None,
            attachments: Vec::new(),
            link_preview: None,
            deleted: false,
            edited: false,
            expires_in: 0,
            expire_start: None,
            edit_count: 0,
            notice: None,
            mentions: Vec::new(),
            styles: Vec::new(),
            cta: None,
            deleted_by_admin: None,
            long_text: None,
            poll: None,
            view_once: None,
            content: None,
            sealed_sender: Vec::new(),
            is_read: true,
        }
    }

    #[test]
    fn a_day_ten_times_text_only() {
        let now = 10 * DAY_MS;
        assert!(can_edit(&ours(now - DAY_MS, "hi"), false, now));
        assert!(!can_edit(&ours(now - DAY_MS - 1, "hi"), false, now));
        assert!(can_edit(&ours(now - 30 * DAY_MS, "hi"), true, now), "Note to Self: any time");
        assert!(!can_edit(&ours(now, ""), false, now), "a photo without a caption");
        let poll = Message { poll: Some(signal_store::Poll::new("Q".into(), vec!["A".into(), "B".into()], false)), ..ours(now, "Q") };
        assert!(!can_edit(&poll, false, now), "a poll");
        assert!(!can_edit(&Message { edit_count: MAX_EDITS, ..ours(now, "hi") }, false, now));
        assert!(!can_edit(&Message { direction: Direction::Incoming, ..ours(now, "hi") }, false, now));
        assert!(!can_edit(&Message { deleted: true, ..ours(now, "hi") }, false, now));
    }
}
