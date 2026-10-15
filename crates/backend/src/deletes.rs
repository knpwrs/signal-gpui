//! Signal's rules for deleting messages for everyone, following Desktop
//! (`ts/util/canDeleteForEveryone.preload.ts`): only the author, never in
//! Note to Self, and only while the message is young. Desktop reads the
//! limit from remote config (`global.normalDeleteMaxAgeInSeconds`) and
//! defaults to a day; receivers allow a further day for slow delivery.

use signal_store::{DeliveryStatus, Direction, Message};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How long after sending you can delete for everyone.
pub const MAX_AGE_MS: i64 = DAY_MS;
/// Extra time a delete may take to arrive.
pub const RECEIVE_GRACE_MS: i64 = DAY_MS;
/// Desktop deletes at most this many messages for everyone at once.
pub const MAX_FOR_EVERYONE: usize = 30;

/// Whether we can still delete our `message` for everyone at `now`.
/// It must have actually gone out to someone.
pub fn can_send(message: &Message, note_to_self: bool, now: i64) -> bool {
    !note_to_self
        && message.direction == Direction::Outgoing
        && !message.deleted
        && matches!(message.status, DeliveryStatus::Sent | DeliveryStatus::Delivered | DeliveryStatus::Read)
        && (now - message.sent_at).abs() <= MAX_AGE_MS
}

/// Whether a delete sent at `deleted_at` by the target's author applies.
/// (The author match is by lookup: deletes only find their sender's messages.)
pub fn can_receive(target: &Message, note_to_self: bool, deleted_at: i64) -> bool {
    !note_to_self && !target.deleted && (deleted_at - target.sent_at).abs() <= MAX_AGE_MS + RECEIVE_GRACE_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use signal_store::ConversationId;

    fn message(direction: Direction, status: DeliveryStatus, sent_at: i64) -> Message {
        Message {
            id: 1,
            conversation_id: ConversationId::direct("a"),
            sent_at,
            received_at: sent_at,
            direction,
            author: None,
            author_name: None,
            body: "hi".into(),
            status,
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
        }
    }

    #[test]
    fn sending_needs_our_recent_sent_message() {
        let now = 10 * DAY_MS;
        let ours = message(Direction::Outgoing, DeliveryStatus::Delivered, now - DAY_MS + 1);
        assert!(can_send(&ours, false, now));
        assert!(!can_send(&ours, true, now), "not in Note to Self");
        assert!(!can_send(&message(Direction::Outgoing, DeliveryStatus::Read, now - DAY_MS - 1), false, now));
        assert!(!can_send(&message(Direction::Outgoing, DeliveryStatus::Sending, now), false, now));
        assert!(!can_send(&message(Direction::Outgoing, DeliveryStatus::Failed, now), false, now));
        assert!(!can_send(&message(Direction::Incoming, DeliveryStatus::Read, now), false, now));
        let deleted = Message { deleted: true, ..ours };
        assert!(!can_send(&deleted, false, now));
    }

    #[test]
    fn receiving_allows_a_day_of_grace() {
        let target = message(Direction::Incoming, DeliveryStatus::Read, 0);
        assert!(can_receive(&target, false, 2 * DAY_MS));
        assert!(!can_receive(&target, false, 2 * DAY_MS + 1));
        assert!(!can_receive(&target, true, 1));
    }
}
