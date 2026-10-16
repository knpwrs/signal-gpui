//! Signal's rules for deleting messages for everyone, following Desktop
//! (`ts/util/canDeleteForEveryone.preload.ts`): only the author, never in
//! Note to Self, and only while the message is young. The limit comes from
//! the remote config (`global.normalDeleteMaxAgeInSeconds`, a day by
//! default); receivers allow a further day for slow delivery.
//!
//! A group admin may also delete anyone's message in the group (Desktop's
//! admin delete, `DataMessage.adminDelete`, behind the remote config's
//! `adminDelete.send` flag), within its own limit
//! (`global.adminDeleteMaxAgeInSeconds`, also a day by default). Our own
//! messages use the normal delete while it's allowed, as in Desktop.

use signal_store::{DeliveryStatus, Direction, Message};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Extra time a delete may take to arrive.
pub const RECEIVE_GRACE_MS: i64 = DAY_MS;

/// How long after sending you can delete for everyone.
pub fn max_age_ms() -> i64 {
    crate::remote_config().normal_delete_max_age_ms()
}

/// How long after a message was sent a group admin can delete it.
pub fn admin_max_age_ms() -> i64 {
    crate::remote_config().admin_delete_max_age_ms()
}
/// Desktop deletes at most this many messages for everyone at once.
pub const MAX_FOR_EVERYONE: usize = 30;

/// Which delete for everyone applies to a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The author's own (`DataMessage.delete`).
    Normal,
    /// A group admin's (`DataMessage.adminDelete`).
    Admin,
}

/// What the rules need to know about a message's chat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Chat {
    pub note_to_self: bool,
    pub group: bool,
    /// We're an admin of the group (and still in it).
    pub we_are_admin: bool,
    /// The remote config lets us send admin deletes
    /// ([`crate::RemoteConfig::admin_delete_send`]).
    pub admin_deletes: bool,
    /// An admin ended the group: no deletes for everyone in it (Desktop's
    /// `canDeleteForEveryone`, "terminated conversation").
    pub ended: bool,
}

impl Chat {
    /// A chat's rules, with admin deletes as the remote config has them.
    pub fn new(note_to_self: bool, group: bool, we_are_admin: bool, ended: bool) -> Self {
        Self { note_to_self, group, we_are_admin, admin_deletes: crate::remote_config().admin_delete_send(), ended }
    }
}

/// Which delete for everyone we can send for `message` at `now`, if any:
/// the normal one for our own messages while it's allowed, else an admin
/// delete (Desktop's `canSendDeleteForEveryone`) when the remote config
/// allows those (`isAdminDeleteSendEnabled`).
pub fn send_kind(message: &Message, chat: Chat, now: i64) -> Option<Kind> {
    if chat.ended {
        return None;
    }
    if can_send(message, chat.note_to_self, now) {
        return Some(Kind::Normal);
    }
    if !chat.admin_deletes {
        return None;
    }
    can_send_as_admin(message, chat, now).ok().map(|()| Kind::Admin)
}

/// Whether we can delete `message` for everyone as a group admin at `now`,
/// or why not. Our own message must have gone out to someone.
pub fn can_send_as_admin(message: &Message, chat: Chat, now: i64) -> Result<(), &'static str> {
    if chat.note_to_self {
        return Err("note to self");
    }
    common(message)?;
    if message.direction == Direction::Outgoing
        && !matches!(message.status, DeliveryStatus::Sent | DeliveryStatus::Delivered | DeliveryStatus::Read)
    {
        return Err("not sent");
    }
    admin(chat.group, chat.we_are_admin, now - message.sent_at, 0)
}

/// Whether a group admin's delete sent at `deleted_at` applies to `target`,
/// or why not (Desktop's `canReceiveDeleteForEveryone` with
/// `isAdminDelete`). `sender_is_admin` is their role when it arrives.
pub fn can_receive_as_admin(target: &Message, group: bool, sender_is_admin: bool, deleted_at: i64) -> Result<(), &'static str> {
    common(target)?;
    admin(group, sender_is_admin, deleted_at - target.sent_at, RECEIVE_GRACE_MS)
}

/// Desktop's `checkCommon`: not already deleted, and a message rather than
/// a timeline notice. (Note to Self and the Signal chat aren't groups.)
fn common(message: &Message) -> Result<(), &'static str> {
    if message.deleted {
        return Err("already deleted");
    }
    if message.notice.is_some() {
        return Err("not a message");
    }
    Ok(())
}

/// Desktop's `canUseAdminDelete`.
fn admin(group: bool, is_admin: bool, age: i64, grace: i64) -> Result<(), &'static str> {
    if !group {
        return Err("not a group");
    }
    if !is_admin {
        return Err("not an admin");
    }
    if age.abs() > admin_max_age_ms() + grace {
        return Err("too old");
    }
    Ok(())
}

/// Whether we can still delete our `message` for everyone at `now`.
/// It must have actually gone out to someone.
pub fn can_send(message: &Message, note_to_self: bool, now: i64) -> bool {
    !note_to_self
        && message.direction == Direction::Outgoing
        && !message.deleted
        && matches!(message.status, DeliveryStatus::Sent | DeliveryStatus::Delivered | DeliveryStatus::Read)
        && (now - message.sent_at).abs() <= max_age_ms()
}

/// Whether a delete sent at `deleted_at` by the target's author applies.
/// (The author match is by lookup: deletes only find their sender's messages.)
pub fn can_receive(target: &Message, note_to_self: bool, deleted_at: i64) -> bool {
    !note_to_self && !target.deleted && (deleted_at - target.sent_at).abs() <= max_age_ms() + RECEIVE_GRACE_MS
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
            long_text: None,
            poll: None,
            view_once: None,
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

    const GROUP: Chat = Chat { note_to_self: false, group: true, we_are_admin: true, admin_deletes: true, ended: false };

    #[test]
    fn admins_delete_anyones_message_in_a_group() {
        let now = 10 * DAY_MS;
        let theirs = message(Direction::Incoming, DeliveryStatus::Read, now - DAY_MS + 1);
        assert_eq!(send_kind(&theirs, GROUP, now), Some(Kind::Admin));
        assert_eq!(send_kind(&theirs, Chat { we_are_admin: false, ..GROUP }, now), None);
        assert_eq!(send_kind(&theirs, Chat { group: false, ..GROUP }, now), None, "only in groups");
        assert_eq!(send_kind(&theirs, Chat { note_to_self: true, ..GROUP }, now), None);
        // Not without the remote config's `adminDelete.send`.
        assert_eq!(send_kind(&theirs, Chat { admin_deletes: false, ..GROUP }, now), None);
        // Nothing in an ended group, even our own.
        assert_eq!(send_kind(&theirs, Chat { ended: true, ..GROUP }, now), None);
        let old = message(Direction::Incoming, DeliveryStatus::Read, now - DAY_MS - 1);
        assert_eq!(can_send_as_admin(&old, GROUP, now), Err("too old"));
        let deleted = Message { deleted: true, ..theirs.clone() };
        assert_eq!(can_send_as_admin(&deleted, GROUP, now), Err("already deleted"));
        let notice = Message { notice: Some(signal_store::Notice::GroupCall { by: None, era: None, call_id: None, status: None, incoming: false, unseen: false }), ..theirs };
        assert_eq!(can_send_as_admin(&notice, GROUP, now), Err("not a message"));
    }

    #[test]
    fn our_own_messages_prefer_the_normal_delete() {
        let now = 10 * DAY_MS;
        let ours = message(Direction::Outgoing, DeliveryStatus::Delivered, now - 1000);
        assert_eq!(send_kind(&ours, GROUP, now), Some(Kind::Normal));
        // Never sent: no normal delete, and no admin delete either.
        let failed = message(Direction::Outgoing, DeliveryStatus::Failed, now - 1000);
        assert_eq!(send_kind(&failed, GROUP, now), None);
    }

    #[test]
    fn admin_deletes_arrive_from_admins_within_two_days() {
        let target = message(Direction::Incoming, DeliveryStatus::Read, 0);
        assert_eq!(can_receive_as_admin(&target, true, true, 2 * DAY_MS), Ok(()));
        assert_eq!(can_receive_as_admin(&target, true, true, 2 * DAY_MS + 1), Err("too old"));
        assert_eq!(can_receive_as_admin(&target, true, false, 1), Err("not an admin"));
        assert_eq!(can_receive_as_admin(&target, false, true, 1), Err("not a group"));
        let deleted = Message { deleted: true, ..target };
        assert_eq!(can_receive_as_admin(&deleted, true, true, 1), Err("already deleted"));
    }
}
