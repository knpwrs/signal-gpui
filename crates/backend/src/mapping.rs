//! Pure translation of decrypted Signal content into database actions.
//!
//! Nothing here touches the network or a store, so it is unit-tested directly;
//! the actor resolves names and applies the actions.

use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::libsignal_service::utils::parse_aci_with_fallback;
use presage::proto::{
    AttachmentPointer, DataMessage, attachment_pointer, EditMessage, ReceiptMessage, SyncMessage, TypingMessage,
    body_range, data_message, receipt_message, sync_message, typing_message,
};
use signal_store::{
    AttachmentMeta, ConversationId, DeliveryStatus, Direction, LinkPreview, NewMessage, Quote, StyleSpan, TextStyle,
};

/// Object replacement character Signal uses as the placeholder for mentions.
pub const MENTION_PLACEHOLDER: char = '\u{FFFC}';

#[derive(Debug, Clone, PartialEq)]
// One short-lived value per received message; boxing `Insert` buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum Action {
    Insert {
        message: NewMessage,
        /// ACIs of mentions, in body order; each replaces one [`MENTION_PLACEHOLDER`].
        mentions: Vec<String>,
        /// Its formatting, over the body as received (placeholders and all).
        styles: Vec<WireStyle>,
        /// Pointers matching `message.attachments`, for downloading.
        pointers: Vec<AttachmentPointer>,
        /// The whole text, when it came as an attachment and `message.body`
        /// is only its start.
        long_text: Option<AttachmentPointer>,
        /// The link preview's image, for downloading.
        preview_image: Option<AttachmentPointer>,
        /// The sender's disappearing-timer version (1:1 chats), with
        /// `message.expires_in` as the timer.
        timer_version: Option<u32>,
        /// A poll (`message.body` is its question).
        poll: Option<signal_store::Poll>,
    },
    React {
        conversation: ConversationId,
        target_author: Option<String>,
        target_sent_at: i64,
        emoji: String,
        /// Reactor; `None` = self.
        author: Option<String>,
        sent_at: i64,
        remove: bool,
    },
    Delete {
        conversation: ConversationId,
        target_author: Option<String>,
        target_sent_at: i64,
        /// When the delete was sent (it only applies to young messages).
        sent_at: i64,
    },
    /// A group admin deleting someone's message for everyone.
    AdminDelete {
        conversation: ConversationId,
        /// The message's author; `None` = self.
        target_author: Option<String>,
        target_sent_at: i64,
        /// The admin; `None` = self (from another of our devices).
        by: Option<String>,
        /// When the delete was sent.
        sent_at: i64,
    },
    Edit {
        conversation: ConversationId,
        target_author: Option<String>,
        target_sent_at: i64,
        /// The edit's own timestamp: replies and reactions to the edited
        /// message name it by this.
        sent_at: i64,
        body: String,
        mentions: Vec<String>,
        styles: Vec<WireStyle>,
        /// The edit's whole text, when it came as an attachment.
        long_text: Option<AttachmentPointer>,
    },
    Receipt {
        sent_at: Vec<i64>,
        status: DeliveryStatus,
    },
    Typing {
        sender: String,
        group_id: Option<Vec<u8>>,
        started: bool,
    },
    /// Our other device read these incoming messages: `(author, sent_at)`.
    ReadSync(Vec<(String, i64)>),
    /// A story, someone's or ours (posted from another device).
    Story {
        story: signal_store::NewStory,
        /// Its photo or video, to download.
        pointer: Option<AttachmentPointer>,
    },
    /// A reply or reaction to a group story, or one to one to our story,
    /// kept with the story.
    StoryReply {
        /// The group, or the chat with whoever replied to ours.
        group: ConversationId,
        /// The story's author; `None` = ours.
        story_author: Option<String>,
        story_sent_at: i64,
        reply: signal_store::StoryReply,
    },
    /// `viewer` viewed our stories sent at these times (a VIEWED receipt).
    StoryViewed { viewer: String, sent_at: Vec<i64> },
    /// Who our story sent at `sent_at` goes to now, from another device.
    StoryRecipientUpdate { sent_at: i64, recipients: Vec<signal_store::StoryRecipient> },
    /// We viewed these on another device: `(author, sent_at)`.
    ViewedSync(Vec<(Option<String>, i64)>),
    /// Someone (`None`: us, on another device) started a group call.
    GroupCallStarted { conversation: ConversationId, by: Option<String>, era: Option<String>, at: i64 },
    /// Another of our devices' record of a 1:1 call.
    CallEvent(CallEventSync),
    /// Another of our devices' record of a group call.
    GroupCallEvent(GroupCallEventSync),
    /// Another of our devices cleared or read its call history.
    CallLogEvent(CallLogEventSync),
    /// Our other device marked a contact's identity key verified (or not).
    Verified { aci: String, identity_key: Vec<u8>, verified: bool },
    /// Our primary device changed the Storage Service (pins, names, ...).
    StorageChanged,
    /// "Delete for me" on another of our devices: these messages
    /// `(chat, author or None for us, sent at)` and whole chats.
    DeleteForMe { messages: Vec<(ThreadRef, Option<String>, i64)>, conversations: Vec<ChatDelete> },
    /// An explicit timer change ("X set the disappearing message time").
    ExpireTimer {
        conversation: ConversationId,
        seconds: u32,
        version: Option<u32>,
        /// Who changed it (`None`: us) and when.
        by: Option<String>,
        at: i64,
    },
    /// A message was pinned. `target_author` `None`: one of ours.
    Pin {
        conversation: ConversationId,
        by: Option<String>,
        target_author: Option<String>,
        target_sent_at: i64,
        /// Seconds; `None` pins forever.
        duration: Option<u32>,
        at: i64,
    },
    Unpin {
        conversation: ConversationId,
        target_author: Option<String>,
        target_sent_at: i64,
    },
    /// A vote in a poll (Desktop's `PollVote`): every option the voter
    /// chooses now, none to take their vote back.
    PollVote {
        conversation: ConversationId,
        /// `None`: us, on another device.
        voter: Option<String>,
        /// The poll's author (`None`: ours) and timestamp.
        target_author: Option<String>,
        target_sent_at: i64,
        options: Vec<u32>,
        vote_count: u32,
        sent_at: i64,
    },
    /// A poll's creator ended it (`by`: `None` for us, on another device).
    PollEnd {
        conversation: ConversationId,
        by: Option<String>,
        target_sent_at: i64,
        sent_at: i64,
    },
}

/// What a group change means here: a notice per change (`by`: who made
/// it, `None` for us), the new timer if it changed, and whether we're out
/// of the group now.
#[derive(Debug, Default, PartialEq)]
pub struct GroupChangeSummary {
    pub notices: Vec<signal_store::Notice>,
    pub timer: Option<u32>,
    pub left: bool,
}

pub fn summarize_group_change(changes: &presage::libsignal_service::groups_v2::GroupChanges, me: &str) -> GroupChangeSummary {
    use presage::libsignal_service::groups_v2::{AccessRequired, GroupChange, Role};
    use signal_store::{GroupNotice, Notice};
    let person = |sid: String| (sid != me).then_some(sid);
    let by = person(changes.editor.service_id_string());
    let admins_only = |access: &AccessRequired| *access == AccessRequired::Administrator;
    let mut summary = GroupChangeSummary::default();
    for change in &changes.changes {
        let notice = match change {
            GroupChange::Title(title) => GroupNotice::Title { title: title.clone() },
            GroupChange::Description(_) => GroupNotice::Description,
            GroupChange::Avatar(_) => GroupNotice::Avatar,
            GroupChange::NewMember(member) => GroupNotice::Added { member: person(member.aci.service_id_string()) },
            GroupChange::PromoteRequestingMember { aci, .. } => GroupNotice::Added { member: person(aci.service_id_string()) },
            GroupChange::DeleteMember(aci) => {
                let member = person(aci.service_id_string());
                summary.left |= member.is_none();
                GroupNotice::Removed { member }
            }
            GroupChange::ModifyMemberRole { aci, role } => {
                GroupNotice::Admin { member: person(aci.service_id_string()), admin: *role == Role::Administrator }
            }
            GroupChange::AttributeAccess(access) => GroupNotice::InfoAccess { admins_only: admins_only(access) },
            GroupChange::MemberAccess(access) => GroupNotice::MemberAccess { admins_only: admins_only(access) },
            GroupChange::AnnouncementOnly(on) => GroupNotice::AnnouncementsOnly { on: *on },
            GroupChange::TerminateGroup => GroupNotice::Ended,
            GroupChange::Timer(timer) => {
                let seconds = timer.as_ref().map_or(0, |t| t.duration);
                summary.timer = Some(seconds);
                summary.notices.push(Notice::Timer { by: by.clone(), seconds });
                continue;
            }
            // Profile keys, invitations, links, bans and labels: no notice.
            _ => continue,
        };
        summary.notices.push(Notice::Group { by: by.clone(), change: notice });
    }
    summary
}

/// The data message an incoming message or our own sync transcript holds.
fn data_message_of(content: &Content) -> Option<&DataMessage> {
    match &content.body {
        ContentBody::DataMessage(dm) => Some(dm),
        ContentBody::SynchronizeMessage(sync) => match &sync.content {
            Some(sync_message::Content::Sent(sent)) => sent.message.as_ref(),
            _ => None,
        },
        _ => None,
    }
}

/// A group change carried by an incoming message (or our own sync
/// transcript): the group's master key, the signed change and when.
pub fn group_change_of(content: &Content) -> Option<([u8; 32], Vec<u8>, i64)> {
    let data = data_message_of(content)?;
    let context = data.group_v2.as_ref()?;
    let change = context.group_change.clone().filter(|c| !c.is_empty())?;
    let key: [u8; 32] = context.master_key.as_deref()?.try_into().ok()?;
    let at = data.timestamp.map_or_else(|| content.metadata.client_timestamp.timestamp_millis(), |t| t as i64);
    Some((key, change, at))
}

/// The group a message was sent in and the group revision its sender had.
/// presage fetches the group again when the revision is newer than its copy.
pub fn group_revision_of(content: &Content) -> Option<([u8; 32], u32)> {
    let context = data_message_of(content)?.group_v2.as_ref()?;
    let key: [u8; 32] = context.master_key.as_deref()?.try_into().ok()?;
    Some((key, context.revision?))
}

/// Whether `me` may set our member label: admins always; members when the
/// group's label access is "members" (Desktop's `getCanAddLabel`).
pub fn can_set_member_label(group: &presage::model::groups::Group, me: &str) -> bool {
    use presage::libsignal_service::groups_v2::{AccessRequired, Role};
    let Some(member) = group.members.iter().find(|m| m.aci.service_id_string() == me) else { return false };
    member.role == Role::Administrator
        || group.access_control.as_ref().is_some_and(|a| a.member_label == AccessRequired::Member)
}

/// Whether `me` may change a group's details (name, timer, pins): its
/// "attributes" access is open to members, or we're an admin.
pub fn can_edit_group_info(group: &presage::model::groups::Group, me: &str) -> bool {
    use presage::libsignal_service::groups_v2::{AccessRequired, Role};
    let Some(member) = group.members.iter().find(|m| m.aci.service_id_string() == me) else { return false };
    match group.access_control.as_ref().map(|a| &a.attributes) {
        Some(AccessRequired::Any | AccessRequired::Member) | None => true,
        Some(AccessRequired::Administrator) => member.role == Role::Administrator,
        Some(AccessRequired::Unknown | AccessRequired::Unsatisfiable) => false,
    }
}

/// The new `(timer, version)` after someone else's timer (Desktop's
/// `updateExpirationTimer`), or `None` to leave `current` alone. In 1:1
/// chats a message's timer applies only if its version is at least ours
/// (version 0 or none means "unknown"); an explicit update always sets the
/// timer. Groups change only by explicit update.
pub fn timer_update(current: (u32, u32), seconds: u32, version: Option<u32>, explicit: bool, group: bool) -> Option<(u32, u32)> {
    let (timer, local) = current;
    let version = version.unwrap_or(0);
    if group && !explicit {
        return None;
    }
    if !explicit && (version == 0 || version < local) {
        return None;
    }
    let next = (seconds, local.max(version));
    (next != (timer, local)).then_some(next)
}

/// A chat as sync messages name it: by service id, or by group id (not the
/// master key we key groups by, so the actor resolves it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadRef {
    Direct(String),
    GroupId(Vec<u8>),
}

fn thread_ref(id: Option<&presage::proto::ConversationIdentifier>) -> Option<ThreadRef> {
    use presage::proto::conversation_identifier::Identifier;
    use presage::libsignal_service::protocol::ServiceId;
    Some(match id?.identifier.as_ref()? {
        Identifier::ThreadServiceId(sid) => ThreadRef::Direct(ServiceId::parse_from_service_id_string(sid)?.service_id_string()),
        Identifier::ThreadServiceIdBinary(bytes) => {
            ThreadRef::Direct(ServiceId::parse_from_service_id_binary(bytes)?.service_id_string())
        }
        Identifier::ThreadGroupId(id) => ThreadRef::GroupId(id.clone()),
        Identifier::ThreadE164(_) => return None,
    })
}

/// A chat deleted on another device. As in Desktop, only messages up to the
/// newest one that device named go (newer ones arrived after it deleted),
/// and the chat itself only if nothing is left. A local-only delete (a chat
/// with nothing addressable in it) names no messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDelete {
    pub thread: ThreadRef,
    /// `(author or None for us, sent at)`, newest first.
    pub most_recent: Vec<(Option<String>, i64)>,
    /// The same among messages without a disappearing timer, in case the
    /// most recent ones have already disappeared here.
    pub most_recent_non_expiring: Vec<(Option<String>, i64)>,
    /// Remove the chat itself once it's empty.
    pub full: bool,
}

/// Who wrote an addressable message: `None` for us.
fn addressable_author(message: &presage::proto::AddressableMessage, ctx: &MapContext) -> Option<Option<String>> {
    use presage::proto::addressable_message::Author;
    use presage::libsignal_service::protocol::ServiceId;
    let sid = match message.author.as_ref()? {
        Author::AuthorServiceId(sid) => ServiceId::parse_from_service_id_string(sid)?,
        Author::AuthorServiceIdBinary(bytes) => ServiceId::parse_from_service_id_binary(bytes)?,
        Author::AuthorE164(_) => return None,
    };
    let sid = sid.service_id_string();
    Some((sid != ctx.self_aci).then_some(sid))
}

pub struct MapContext<'a> {
    pub self_aci: &'a str,
    /// Unix millis used as `received_at`.
    pub now: i64,
}

enum Origin {
    Incoming { sender: String },
    Outgoing { destination: Option<String> },
}

fn aci_string(aci: Option<Aci>) -> Option<String> {
    aci.map(|a| a.service_id_string())
}

/// `None` when `id` is our own ACI (the store's convention for "self").
fn other(id: Option<String>, ctx: &MapContext) -> Option<String> {
    id.filter(|id| id != ctx.self_aci)
}

pub fn map_content(content: &Content, ctx: &MapContext) -> Vec<Action> {
    let sender = content.metadata.sender.service_id_string();
    let envelope_ts = content.metadata.client_timestamp.timestamp_millis();
    match &content.body {
        ContentBody::DataMessage(dm) => {
            let ts = dm.timestamp.map(|t| t as i64).unwrap_or(envelope_ts);
            map_data_message(dm, Origin::Incoming { sender }, ts, ctx)
        }
        ContentBody::EditMessage(edit) => {
            map_edit(edit, Origin::Incoming { sender }, envelope_ts, ctx).into_iter().collect()
        }
        ContentBody::SynchronizeMessage(sync) => map_sync(sync, envelope_ts, ctx),
        ContentBody::ReceiptMessage(receipt) => {
            let mut out: Vec<Action> = map_receipt(receipt).into_iter().collect();
            // A VIEWED receipt is also how a story's views are counted.
            if receipt.r#type() == receipt_message::Type::Viewed && !receipt.timestamp.is_empty() {
                out.push(Action::StoryViewed { viewer: sender, sent_at: receipt.timestamp.iter().map(|t| *t as i64).collect() });
            }
            out
        }
        ContentBody::StoryMessage(story) => map_story(story, Some(sender), envelope_ts, ctx).into_iter().collect(),
        ContentBody::TypingMessage(typing) => vec![map_typing(typing, sender)],
        _ => Vec::new(),
    }
}

fn conversation_for(dm: &DataMessage, origin: &Origin) -> Option<ConversationId> {
    if let Some(key) = dm.group_v2.as_ref().and_then(|g| g.master_key.as_deref()) {
        return (key.len() == 32).then(|| ConversationId::group(key));
    }
    match origin {
        Origin::Incoming { sender } => Some(ConversationId::direct(sender)),
        Origin::Outgoing { destination } => destination.as_deref().map(ConversationId::direct),
    }
}

fn author_of(origin: &Origin, ctx: &MapContext) -> Option<String> {
    match origin {
        Origin::Incoming { sender } => other(Some(sender.clone()), ctx),
        Origin::Outgoing { .. } => None,
    }
}

fn mentions(ranges: &[presage::proto::BodyRange]) -> Vec<String> {
    let mut found: Vec<(u32, String)> = ranges
        .iter()
        .filter_map(|r| {
            let aci = match &r.associated_value {
                Some(body_range::AssociatedValue::MentionAci(s)) => Aci::parse_from_service_id_string(s),
                Some(body_range::AssociatedValue::MentionAciBinary(b)) => parse_aci_with_fallback(Some(b), None),
                _ => None,
            }?;
            Some((r.start.unwrap_or(0), aci.service_id_string()))
        })
        .collect();
    found.sort_by_key(|(start, _)| *start);
    found.into_iter().map(|(_, aci)| aci).collect()
}

/// A style as Signal sends it: UTF-16 positions in the body as sent, where
/// each mention is one placeholder.
pub use signal_store::WireStyle;

pub(crate) fn styles(ranges: &[presage::proto::BodyRange]) -> Vec<WireStyle> {
    use presage::proto::body_range::{AssociatedValue, Style};
    ranges
        .iter()
        .filter_map(|r| {
            let Some(AssociatedValue::Style(style)) = r.associated_value else { return None };
            let style = match Style::try_from(style).ok()? {
                Style::Bold => TextStyle::Bold,
                Style::Italic => TextStyle::Italic,
                Style::Spoiler => TextStyle::Spoiler,
                Style::Strikethrough => TextStyle::Strikethrough,
                Style::Monospace => TextStyle::Monospace,
                Style::None => return None,
            };
            Some(WireStyle { start: r.start.unwrap_or(0), len: r.length.unwrap_or(0), style })
        })
        .filter(|s| s.len > 0)
        .collect()
}

/// Wire styles as byte spans over the same text (placeholders left as
/// they are).
fn byte_spans(text: &str, styles: &[WireStyle]) -> Vec<StyleSpan> {
    let mut at: Vec<(u32, usize)> = Vec::new();
    let mut utf16 = 0u32;
    for (byte, c) in text.char_indices() {
        at.push((utf16, byte));
        utf16 += c.len_utf16() as u32;
    }
    at.push((utf16, text.len()));
    place_styles(&at, styles)
}

/// Styles placed through `at`: (UTF-16 position as sent, byte where it
/// lands), in order. A position inside a character (half a surrogate pair)
/// rounds outward.
fn place_styles(at: &[(u32, usize)], styles: &[WireStyle]) -> Vec<StyleSpan> {
    let byte = |pos: u32, up: bool| match at.binary_search_by_key(&pos, |(u, _)| *u) {
        Ok(ix) => Some(at[ix].1),
        // Past the end (the start of a long text, say): cut at the end.
        Err(ix) if up && ix == at.len() => at.last().map(|(_, b)| *b),
        Err(ix) => at.get(if up { ix } else { ix.checked_sub(1)? }).map(|(_, b)| *b),
    };
    styles
        .iter()
        .filter_map(|s| {
            let start = byte(s.start, false)?;
            let end = byte(s.start.saturating_add(s.len), true)?;
            (end > start).then_some(StyleSpan { start, len: end - start, style: s.style })
        })
        .collect()
}

/// Signal's ranges for these styles.
pub fn style_ranges(styles: &[WireStyle]) -> Vec<presage::proto::BodyRange> {
    use presage::proto::body_range::{AssociatedValue, Style};
    styles
        .iter()
        .map(|s| presage::proto::BodyRange {
            start: Some(s.start),
            length: Some(s.len),
            associated_value: Some(AssociatedValue::Style(
                match s.style {
                    TextStyle::Bold => Style::Bold,
                    TextStyle::Italic => Style::Italic,
                    TextStyle::Spoiler => Style::Spoiler,
                    TextStyle::Strikethrough => Style::Strikethrough,
                    TextStyle::Monospace => Style::Monospace,
                }
                .into(),
            )),
        })
        .collect()
}

/// Byte spans over `body` (as sent: placeholders and all) in UTF-16, as
/// Signal counts.
pub fn wire_styles(body: &str, spans: &[StyleSpan]) -> Vec<WireStyle> {
    let utf16 = |byte: usize| body.get(..byte).map(|b| b.encode_utf16().count() as u32);
    spans
        .iter()
        .filter_map(|s| {
            let start = utf16(s.start)?;
            let end = utf16(s.start + s.len)?;
            (end > start).then_some(WireStyle { start, len: end - start, style: s.style })
        })
        .collect()
}

pub fn attachment_meta(p: &AttachmentPointer) -> AttachmentMeta {
    AttachmentMeta {
        content_type: p
            .content_type
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "application/octet-stream".into()),
        file_name: p.file_name.clone(),
        size: p.size.unwrap_or(0).into(),
        width: p.width.filter(|w| *w > 0),
        height: p.height.filter(|h| *h > 0),
        local_path: None,
        sticker: false,
        gif: p.flags.unwrap_or(0) & attachment_pointer::Flags::Gif as u32 != 0,
        voice: p.flags.unwrap_or(0) & attachment_pointer::Flags::VoiceMessage as u32 != 0,
        remote: None,
    }
}

/// What a message's whole text is sent as when it's too long for the body
/// (Desktop's `LONG_MESSAGE`, `ts/types/MIME.std.ts`).
pub const LONG_TEXT_TYPE: &str = "text/x-signal-plain";

fn is_long_text(pointer: &AttachmentPointer) -> bool {
    pointer.content_type.as_deref() == Some(LONG_TEXT_TYPE)
}

/// A message's attachments, without its long text (Desktop's
/// `partitionBodyAndNormalAttachments`, `ts/util/Attachment.std.ts`).
pub fn normal_attachments(dm: &DataMessage) -> impl Iterator<Item = &AttachmentPointer> {
    dm.attachments.iter().filter(|p| !is_long_text(p))
}

/// A message's whole text, when it came as an attachment: the first such
/// attachment, as in Desktop (`partitionBodyAndNormalAttachments`).
pub fn long_text_of(dm: &DataMessage) -> Option<AttachmentPointer> {
    dm.attachments.iter().find(|p| is_long_text(p)).cloned()
}

/// Where to download an attachment from Signal's transit CDN.
pub fn remote_of(pointer: &AttachmentPointer) -> Option<signal_store::RemoteAttachment> {
    use attachment_pointer::AttachmentIdentifier;
    let cdn_key = match pointer.attachment_identifier.as_ref()? {
        AttachmentIdentifier::CdnId(id) => id.to_string(),
        AttachmentIdentifier::CdnKey(key) if !key.is_empty() => key.clone(),
        AttachmentIdentifier::CdnKey(_) => return None,
    };
    let key = pointer.key.as_deref().filter(|k| k.len() == 64)?;
    let digest = pointer.digest.as_deref().filter(|d| !d.is_empty())?;
    Some(signal_store::RemoteAttachment {
        cdn: pointer.cdn_number.unwrap_or(0),
        cdn_key,
        key: hex::encode(key),
        digest: Some(hex::encode(digest)),
        plaintext_hash: None,
    })
}

/// Body text for a data message, including fallbacks for content types the
/// UI doesn't render natively.
fn body_text(dm: &DataMessage) -> String {
    if let Some(body) = dm.body.as_deref().filter(|b| !b.is_empty()) {
        return body.to_owned();
    }
    if let Some(sticker) = &dm.sticker {
        return sticker.emoji.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "Sticker".into());
    }
    if let Some(poll) = &dm.poll_create {
        return poll.question.clone().unwrap_or_default();
    }
    if dm.payment.is_some() {
        return "💸 Payment".into();
    }
    if dm.gift_badge.is_some() {
        return "🎁 Gift badge".into();
    }
    if !dm.contact.is_empty() {
        return "👤 Contact".into();
    }
    String::new()
}

fn map_data_message(dm: &DataMessage, origin: Origin, ts: i64, ctx: &MapContext) -> Vec<Action> {
    let Some(conversation) = conversation_for(dm, &origin) else {
        return Vec::new();
    };
    let author = author_of(&origin, ctx);
    let flags = dm.flags.unwrap_or(0);

    if flags & data_message::Flags::ExpirationTimerUpdate as u32 != 0 {
        return vec![Action::ExpireTimer {
            conversation,
            seconds: dm.expire_timer.unwrap_or(0),
            version: dm.expire_timer_version,
            by: author,
            at: ts,
        }];
    }

    // Desktop's "… started a group call" (sent by each who joins; the era
    // tells one call from the next).
    if let Some(update) = &dm.group_call_update
        && conversation.as_group().is_some()
    {
        return vec![Action::GroupCallStarted { conversation, by: author, era: update.era_id.clone(), at: ts }];
    }

    // Pins name their target by ACI; `None` when it's one of ours.
    let target_author = |binary: &Option<Vec<u8>>| -> Option<Option<String>> {
        let aci = presage::libsignal_service::protocol::Aci::parse_from_service_id_binary(binary.as_deref()?)?;
        Some(other(Some(aci.service_id_string()), ctx))
    };
    if let Some(pin) = &dm.pin_message {
        use data_message::pin_message::PinDuration;
        let (Some(target), Some(sent_at)) = (target_author(&pin.target_author_aci_binary), pin.target_sent_timestamp) else {
            return Vec::new();
        };
        let duration = match pin.pin_duration {
            Some(PinDuration::PinDurationSeconds(seconds)) => Some(seconds),
            Some(PinDuration::PinDurationForever(_)) | None => None,
        };
        return vec![Action::Pin {
            conversation,
            by: author,
            target_author: target,
            target_sent_at: sent_at as i64,
            duration,
            at: ts,
        }];
    }
    if let Some(unpin) = &dm.unpin_message {
        let (Some(target), Some(sent_at)) = (target_author(&unpin.target_author_aci_binary), unpin.target_sent_timestamp)
        else {
            return Vec::new();
        };
        return vec![Action::Unpin { conversation, target_author: target, target_sent_at: sent_at as i64 }];
    }

    // Polls (Desktop's `background.preload.ts` and `PollVoteSchema` /
    // `PollTerminateSchema`): a vote names its poll's author and a
    // positive timestamp, and options 0 to 9; anything else is dropped.
    if let Some(vote) = &dm.poll_vote {
        let (Some(target), Some(sent_at)) =
            (target_author(&vote.target_author_aci_binary), vote.target_sent_timestamp.filter(|t| *t > 0))
        else {
            return Vec::new();
        };
        let Some(options) = signal_store::vote_options(&vote.option_indexes) else { return Vec::new() };
        return vec![Action::PollVote {
            conversation,
            voter: author,
            target_author: target,
            target_sent_at: sent_at as i64,
            options,
            vote_count: vote.vote_count.unwrap_or(0),
            sent_at: ts,
        }];
    }
    if let Some(end) = &dm.poll_terminate {
        let Some(target) = end.target_sent_timestamp.filter(|t| *t > 0) else { return Vec::new() };
        return vec![Action::PollEnd { conversation, by: author, target_sent_at: target as i64, sent_at: ts }];
    }

    if let Some(context) = &dm.story_context {
        return map_story_reply(dm, context, conversation, author, ts, ctx);
    }

    if let Some(reaction) = &dm.reaction {
        let (Some(emoji), Some(target)) = (reaction.emoji.clone(), reaction.target_sent_timestamp)
        else {
            return Vec::new();
        };
        return vec![Action::React {
            conversation,
            target_author: other(aci_string(reaction.parse_target_author_aci()), ctx),
            target_sent_at: target as i64,
            emoji,
            author,
            sent_at: ts,
            remove: reaction.remove.unwrap_or(false),
        }];
    }

    // Desktop's `processAdminDelete` needs both the author and the time.
    if let Some(delete) = &dm.admin_delete {
        let (Some(target), Some(sent_at)) = (target_author(&delete.target_author_aci_binary), delete.target_sent_timestamp)
        else {
            return Vec::new();
        };
        return vec![Action::AdminDelete {
            conversation,
            target_author: target,
            target_sent_at: sent_at as i64,
            by: author,
            sent_at: ts,
        }];
    }

    if let Some(target) = dm.delete.as_ref().and_then(|d| d.target_sent_timestamp) {
        return vec![Action::Delete {
            conversation,
            target_author: author,
            target_sent_at: target as i64,
            sent_at: ts,
        }];
    }

    // A poll Desktop wouldn't accept (`PollCreateSchema`) drops the whole
    // message, as Desktop's `handleDataMessage` does.
    let poll = match &dm.poll_create {
        Some(create) => {
            let question = create.question.clone().unwrap_or_default();
            if !signal_store::valid_poll(&question, &create.options) {
                return Vec::new();
            }
            Some(signal_store::Poll::new(question, create.options.clone(), create.allow_multiple.unwrap_or(false)))
        }
        None => None,
    };
    let body = match &poll {
        Some(poll) => poll.question.clone(),
        None => body_text(dm),
    };
    let sticker = dm.sticker.as_ref().and_then(|s| s.data.clone()).filter(|_| dm.body.is_none());
    let is_sticker = sticker.is_some();
    let pointers: Vec<AttachmentPointer> = normal_attachments(dm).cloned().chain(sticker).collect();
    // A poll's body is its question, never a long text.
    let long_text = if poll.is_some() { None } else { long_text_of(dm) };
    if body.is_empty() && pointers.is_empty() && long_text.is_none() {
        // Profile key updates, group changes, call updates, etc.
        return Vec::new();
    }

    let quote = dm.quote.as_ref().and_then(|q| {
        Some(Quote {
            author: other(aci_string(q.parse_author_aci()), ctx),
            author_name: String::new(),
            sent_at: q.id? as i64,
            // A quoted spoiler stays hidden (as Desktop shows it).
            text: signal_store::hide_spoilers(
                q.text.as_deref().unwrap_or_default(),
                &byte_spans(q.text.as_deref().unwrap_or_default(), &styles(&q.body_ranges)),
            ),
            // Filled in from the quoted message once it's found locally.
            thumbnail: None,
            attachment: q.attachments.first().map(|a| signal_store::QuotedAttachment {
                content_type: a.content_type.clone().unwrap_or_default(),
                file_name: a.file_name.clone(),
                ..Default::default()
            }),
            story: false,
        })
    });

    // Desktop shows the first preview, and only for a link in the message.
    let preview = dm
        .preview
        .first()
        .filter(|p| p.url.as_deref().is_some_and(|url| crate::link_preview::is_valid(&body, url)));
    let link_preview = preview.map(|p| LinkPreview {
        url: p.url.clone().unwrap_or_default(),
        title: p.title.clone().filter(|t| !t.trim().is_empty()),
        description: p.description.clone().filter(|d| !d.trim().is_empty()),
        date: p.date.filter(|d| *d > 0).map(|d| d as i64),
        image: p.image.as_ref().map(attachment_meta),
    });
    let preview_image = preview.and_then(|p| p.image.clone());

    let outgoing = matches!(origin, Origin::Outgoing { .. });
    vec![Action::Insert {
        message: NewMessage {
            conversation_id: conversation,
            sent_at: ts,
            received_at: ctx.now,
            direction: if outgoing { Direction::Outgoing } else { Direction::Incoming },
            author,
            author_name: None,
            body,
            status: if outgoing { DeliveryStatus::Sent } else { DeliveryStatus::Delivered },
            quote,
            attachments: {
                let mut metas: Vec<AttachmentMeta> = pointers.iter().map(attachment_meta).collect();
                if is_sticker && let Some(last) = metas.last_mut() {
                    last.sticker = true;
                }
                metas
            },
            link_preview,
            expires_in: dm.expire_timer.unwrap_or(0),
        },
        mentions: if poll.is_some() { Vec::new() } else { mentions(&dm.body_ranges) },
        styles: if poll.is_some() { Vec::new() } else { styles(&dm.body_ranges) },
        pointers,
        long_text,
        preview_image,
        timer_version: dm.expire_timer_version,
        poll,
    }]
}

/// A story (Desktop's `#handleStoryMessage`): the first attachment or a
/// text story, which must have text or a link. `author` is `None` for ours.
fn map_story(story: &presage::proto::StoryMessage, author: Option<String>, sent_at: i64, ctx: &MapContext) -> Option<Action> {
    use presage::proto::story_message::Attachment;
    use signal_store::{StoryContent, TextStory};
    let author = other(author, ctx);
    let group = story
        .group
        .as_ref()
        .and_then(|g| g.master_key.as_deref())
        .filter(|k| k.len() == 32)
        .map(ConversationId::group);
    let (content, pointer) = match story.attachment.as_ref()? {
        Attachment::FileAttachment(pointer) => {
            let caption = pointer.caption.clone().unwrap_or_default();
            let styles = byte_spans(&caption, &styles(&story.body_ranges));
            (StoryContent::Media { attachment: attachment_meta(pointer), caption, styles }, Some(pointer.clone()))
        }
        Attachment::TextAttachment(text) => {
            let preview = text.preview.as_ref().filter(|p| p.url.as_deref().is_some_and(|u| !u.is_empty())).map(|p| LinkPreview {
                url: p.url.clone().unwrap_or_default(),
                title: p.title.clone().filter(|t| !t.trim().is_empty()),
                description: p.description.clone().filter(|d| !d.trim().is_empty()),
                date: p.date.filter(|d| *d > 0).map(|d| d as i64),
                image: None,
            });
            let body = text.text.clone().unwrap_or_default();
            if body.trim().is_empty() && preview.is_none() {
                return None;
            }
            (
                StoryContent::Text(TextStory {
                    text: body,
                    font: story_font(text.text_style()),
                    foreground: text.text_foreground_color,
                    text_background: text.text_background_color,
                    background: story_background(text.background.as_ref()),
                    preview,
                }),
                None,
            )
        }
    };
    Some(Action::Story {
        story: signal_store::NewStory {
            read: author.is_none(),
            author,
            author_name: None,
            sent_at,
            received_at: ctx.now,
            group,
            content,
            pointer: None,
            allows_replies: story.allows_replies.unwrap_or(false),
            recipients: Vec::new(),
        },
        pointer,
    })
}

fn story_font(style: presage::proto::text_attachment::Style) -> signal_store::StoryFont {
    use presage::proto::text_attachment::Style;
    use signal_store::StoryFont;
    match style {
        Style::Default => StoryFont::Default,
        Style::Regular => StoryFont::Regular,
        Style::Bold => StoryFont::Bold,
        Style::Serif => StoryFont::Serif,
        Style::Script => StoryFont::Script,
        Style::Condensed => StoryFont::Condensed,
    }
}

/// Desktop's `getBackgroundColor`: a gradient's colors at their positions,
/// else its start and end (when the lists don't match), else the color,
/// else white.
fn story_background(background: Option<&presage::proto::text_attachment::Background>) -> signal_store::StoryBackground {
    use presage::proto::text_attachment::Background;
    use signal_store::StoryBackground;
    match background {
        Some(Background::Color(color)) => StoryBackground::Color(*color),
        Some(Background::Gradient(gradient)) => {
            let angle = gradient.angle.unwrap_or(0);
            if !gradient.colors.is_empty() && gradient.colors.len() == gradient.positions.len() {
                StoryBackground::Gradient { colors: gradient.colors.clone(), positions: gradient.positions.clone(), angle }
            } else {
                let start = gradient.start_color.unwrap_or(0xffff_ffff);
                let end = gradient.end_color.unwrap_or(start);
                StoryBackground::Gradient { colors: vec![start, end], positions: vec![0., 1.], angle }
            }
        }
        None => StoryBackground::default(),
    }
}

/// A reply or reaction to a story (`DataMessage.storyContext`). In a group
/// it stays with the story; one to one, it's a message in the chat, quoting
/// the story (Desktop's "Story" quote, and "Reacted to …" for a reaction).
fn map_story_reply(
    dm: &DataMessage,
    context: &data_message::StoryContext,
    conversation: ConversationId,
    author: Option<String>,
    ts: i64,
    ctx: &MapContext,
) -> Vec<Action> {
    let story_author = other(
        parse_aci_with_fallback(context.author_aci_binary.as_deref(), context.author_aci.as_deref()).map(|a| a.service_id_string()),
        ctx,
    );
    let Some(story_sent_at) = context.sent_timestamp.map(|t| t as i64) else { return Vec::new() };
    let emoji = dm.reaction.as_ref().filter(|r| !r.remove.unwrap_or(false)).and_then(|r| r.emoji.clone());
    if dm.reaction.is_some() && emoji.is_none() {
        // A reaction taken back: nothing to show.
        return Vec::new();
    }
    let body = match &emoji {
        Some(emoji) => emoji.clone(),
        None => body_text(dm),
    };
    if conversation.as_group().is_some() {
        return vec![Action::StoryReply {
            group: conversation,
            story_author,
            story_sent_at,
            reply: signal_store::StoryReply { author, author_name: None, sent_at: ts, body, emoji },
        }];
    }
    let outgoing = author.is_none();
    // A reply to our story is also kept with it, for its Replies list
    // (Desktop's `loadStoryReplies` finds a story's direct replies too).
    let mut out = Vec::new();
    if story_author.is_none() && !outgoing {
        out.push(Action::StoryReply {
            group: conversation.clone(),
            story_author: None,
            story_sent_at,
            reply: signal_store::StoryReply { author: author.clone(), author_name: None, sent_at: ts, body: body.clone(), emoji: emoji.clone() },
        });
    }
    let text = match (&emoji, story_author.is_none()) {
        (None, _) => "Story".to_owned(),
        (Some(_), true) => "Reacted to your story".to_owned(),
        (Some(_), false) => "Reacted to a story".to_owned(),
    };
    out.push(Action::Insert {
        message: NewMessage {
            conversation_id: conversation,
            sent_at: ts,
            received_at: ctx.now,
            direction: if outgoing { Direction::Outgoing } else { Direction::Incoming },
            author,
            author_name: None,
            body,
            status: if outgoing { DeliveryStatus::Sent } else { DeliveryStatus::Delivered },
            quote: Some(Quote {
                author: story_author,
                author_name: String::new(),
                sent_at: story_sent_at,
                text,
                thumbnail: None,
                attachment: None,
                story: true,
            }),
            attachments: Vec::new(),
            link_preview: None,
            expires_in: dm.expire_timer.unwrap_or(0),
        },
        mentions: if emoji.is_some() { Vec::new() } else { mentions(&dm.body_ranges) },
        styles: if emoji.is_some() { Vec::new() } else { styles(&dm.body_ranges) },
        pointers: Vec::new(),
        long_text: if emoji.is_some() { None } else { long_text_of(dm) },
        preview_image: None,
        timer_version: dm.expire_timer_version,
        poll: None,
    });
    out
}

/// `envelope_ts` dates the edit when its message carries no timestamp.
fn map_edit(edit: &EditMessage, origin: Origin, envelope_ts: i64, ctx: &MapContext) -> Option<Action> {
    let target = edit.target_sent_timestamp? as i64;
    let dm = edit.data_message.as_ref()?;
    let conversation = conversation_for(dm, &origin)?;
    Some(Action::Edit {
        conversation,
        target_author: author_of(&origin, ctx),
        target_sent_at: target,
        sent_at: dm.timestamp.map_or(envelope_ts, |t| t as i64),
        body: dm.body.clone().unwrap_or_default(),
        mentions: mentions(&dm.body_ranges),
        styles: styles(&dm.body_ranges),
        long_text: long_text_of(dm),
    })
}

/// A sent transcript's story recipients, the binary service id preferred.
fn story_recipients(recipients: &[sync_message::sent::StoryMessageRecipient]) -> Vec<signal_store::StoryRecipient> {
    recipients
        .iter()
        .filter_map(|r| {
            let sid = r
                .destination_service_id_binary
                .as_deref()
                .and_then(ServiceId::parse_from_service_id_binary)
                .or_else(|| r.destination_service_id.as_deref().and_then(ServiceId::parse_from_service_id_string))?;
            Some(signal_store::StoryRecipient {
                service_id: sid.service_id_string(),
                list_ids: r.distribution_list_ids.clone(),
                allows_replies: r.is_allowed_to_reply.unwrap_or(false),
            })
        })
        .collect()
}

fn map_sync(sync: &SyncMessage, envelope_ts: i64, ctx: &MapContext) -> Vec<Action> {
    let mut out = Vec::new();
    if let Some(sync_message::Content::Sent(sent)) = &sync.content {
        let destination = sent.parse_destination_service_id().map(|s| s.service_id_string());
        let ts = sent.timestamp.map(|t| t as i64).unwrap_or(envelope_ts);
        if let Some(dm) = &sent.message {
            out.extend(map_data_message(
                dm,
                Origin::Outgoing { destination: destination.clone() },
                ts,
                ctx,
            ));
        }
        if let Some(edit) = &sent.edit_message {
            out.extend(map_edit(edit, Origin::Outgoing { destination }, ts, ctx));
        }
        let recipients = story_recipients(&sent.story_message_recipients);
        // A story we posted from another device.
        if let Some(story) = &sent.story_message {
            out.extend(map_story(story, None, ts, ctx).map(|mut action| {
                if let Action::Story { story, .. } = &mut action {
                    story.recipients = recipients;
                }
                action
            }));
        } else if sent.is_recipient_update() {
            // Who one of our stories goes to changed on another device
            // (Desktop's `onStoryRecipientUpdate`): a story list was
            // deleted, or the story was deleted for everyone.
            out.push(Action::StoryRecipientUpdate { sent_at: ts, recipients });
        }
    }
    let viewed: Vec<(Option<String>, i64)> = sync
        .viewed
        .iter()
        .filter_map(|v| {
            let aci = parse_aci_with_fallback(v.sender_aci_binary.as_deref(), v.sender_aci.as_deref())?;
            Some((other(Some(aci.service_id_string()), ctx), v.timestamp? as i64))
        })
        .collect();
    if !viewed.is_empty() {
        out.push(Action::ViewedSync(viewed));
    }
    let reads: Vec<(String, i64)> = sync
        .read
        .iter()
        .filter_map(|r| {
            let aci = parse_aci_with_fallback(r.sender_aci_binary.as_deref(), r.sender_aci.as_deref())?;
            Some((aci.service_id_string(), r.timestamp? as i64))
        })
        .collect();
    if !reads.is_empty() {
        out.push(Action::ReadSync(reads));
    }
    if let Some(sync_message::Content::CallEvent(event)) = &sync.content {
        if let Some(sync) = call_event(event) {
            out.push(Action::CallEvent(sync));
        } else if let Some(sync) = group_call_event(event) {
            out.push(Action::GroupCallEvent(sync));
        }
    }
    if let Some(sync_message::Content::CallLogEvent(event)) = &sync.content
        && let Some(sync) = call_log_event(event, envelope_ts)
    {
        out.push(Action::CallLogEvent(sync));
    }
    if let Some(sync_message::Content::Verified(v)) = &sync.content
        && let Some(aci) = parse_aci_with_fallback(v.destination_aci_binary.as_deref(), v.destination_aci.as_deref())
        && let Some(identity_key) = v.identity_key.clone()
    {
        out.push(Action::Verified {
            aci: aci.service_id_string(),
            identity_key,
            verified: v.state() == presage::proto::verified::State::Verified,
        });
    }
    if let Some(sync_message::Content::DeleteForMe(delete)) = &sync.content {
        let messages: Vec<_> = delete
            .message_deletes
            .iter()
            .flat_map(|deletes| {
                let thread = thread_ref(deletes.conversation.as_ref());
                deletes.messages.iter().filter_map(move |m| {
                    Some((thread.clone()?, addressable_author(m, ctx)?, m.sent_timestamp? as i64))
                })
            })
            .collect();
        let named = |messages: &[presage::proto::AddressableMessage]| -> Vec<(Option<String>, i64)> {
            messages.iter().filter_map(|m| Some((addressable_author(m, ctx)?, m.sent_timestamp? as i64))).collect()
        };
        let conversations: Vec<_> = delete
            .conversation_deletes
            .iter()
            .filter_map(|d| {
                Some(ChatDelete {
                    thread: thread_ref(d.conversation.as_ref())?,
                    most_recent: named(&d.most_recent_messages),
                    most_recent_non_expiring: named(&d.most_recent_non_expiring_messages),
                    full: d.is_full_delete.unwrap_or(false),
                })
            })
            .chain(delete.local_only_conversation_deletes.iter().filter_map(|d| {
                Some(ChatDelete {
                    thread: thread_ref(d.conversation.as_ref())?,
                    most_recent: Vec::new(),
                    most_recent_non_expiring: Vec::new(),
                    full: true,
                })
            }))
            .collect();
        if !messages.is_empty() || !conversations.is_empty() {
            out.push(Action::DeleteForMe { messages, conversations });
        }
    }
    if let Some(sync_message::Content::FetchLatest(fetch)) = &sync.content
        && fetch.r#type() == sync_message::fetch_latest::Type::StorageManifest
    {
        out.push(Action::StorageChanged);
    }
    out
}

fn map_receipt(receipt: &ReceiptMessage) -> Option<Action> {
    let status = match receipt_message::Type::try_from(receipt.r#type.unwrap_or_default()).ok()? {
        receipt_message::Type::Delivery => DeliveryStatus::Delivered,
        receipt_message::Type::Read | receipt_message::Type::Viewed => DeliveryStatus::Read,
    };
    let sent_at: Vec<i64> = receipt.timestamp.iter().map(|t| *t as i64).collect();
    (!sent_at.is_empty()).then_some(Action::Receipt { sent_at, status })
}

fn map_typing(typing: &TypingMessage, sender: String) -> Action {
    Action::Typing {
        sender,
        group_id: typing.group_id.clone().filter(|g| !g.is_empty()),
        started: typing.action() == typing_message::Action::Started,
    }
}

/// Replaces each mention placeholder, in order, with `@name`.
pub fn render_mentions(body: &str, names: &[String]) -> String {
    let acis = vec![String::new(); names.len()];
    render_mention_spans(body, &acis, names).0
}

/// Replaces each placeholder with "@Name" and says where each landed.
pub fn render_mention_spans(body: &str, acis: &[String], names: &[String]) -> (String, Vec<signal_store::MentionSpan>) {
    let (body, mentions, _) = render_body(body, acis, names, &[]);
    (body, mentions)
}

/// The body as kept: each placeholder as "@Name", with where each mention
/// and each style (given over the body as sent) lands in it.
pub fn render_body(
    body: &str,
    acis: &[String],
    names: &[String],
    styles: &[WireStyle],
) -> (String, Vec<signal_store::MentionSpan>, Vec<StyleSpan>) {
    // Where each UTF-16 position of the body as sent lands, in bytes.
    let mut at: Vec<(u32, usize)> = Vec::with_capacity(body.len() + 1);
    let mut utf16 = 0u32;
    let mut out = String::with_capacity(body.len());
    let mut spans = Vec::new();
    let mut ix = 0;
    for c in body.chars() {
        at.push((utf16, out.len()));
        utf16 += c.len_utf16() as u32;
        if c == MENTION_PLACEHOLDER {
            let start = out.len();
            out.push('@');
            out.push_str(names.get(ix).map(String::as_str).unwrap_or("Unknown"));
            if let Some(aci) = acis.get(ix).filter(|a| !a.is_empty()) {
                spans.push(signal_store::MentionSpan { start, len: out.len() - start, aci: aci.clone() });
            }
            ix += 1;
        } else {
            out.push(c);
        }
    }
    at.push((utf16, out.len()));
    let styles = place_styles(&at, styles);
    (out, spans, styles)
}

/// The reverse of [`render_mention_spans`]: each "@Name" span back to a
/// placeholder, and who each one mentions, in order.
pub fn placeholders_for(body: &str, spans: &[signal_store::MentionSpan]) -> (String, Vec<String>) {
    let mut spans: Vec<&signal_store::MentionSpan> = spans.iter().collect();
    spans.sort_by_key(|s| s.start);
    let mut out = String::with_capacity(body.len());
    let mut acis = Vec::new();
    let mut last = 0;
    for span in spans {
        let (Some(before), true) = (body.get(last..span.start), body.get(span.start..span.start + span.len).is_some())
        else {
            continue;
        };
        out.push_str(before);
        out.push(MENTION_PLACEHOLDER);
        acis.push(span.aci.clone());
        last = span.start + span.len;
    }
    out.push_str(body.get(last..).unwrap_or_default());
    (out, acis)
}

/// Signal's mention ranges for a body with a placeholder per mention
/// (`acis` in order): one UTF-16 position each, length 1.
pub fn mention_ranges(body: &str, acis: &[String]) -> Vec<presage::proto::BodyRange> {
    use presage::proto::body_range::AssociatedValue;
    let mut ranges = Vec::new();
    let mut utf16 = 0u32;
    let mut acis = acis.iter();
    for c in body.chars() {
        if c == MENTION_PLACEHOLDER
            && let Some(aci) = acis.next()
        {
            ranges.push(presage::proto::BodyRange {
                start: Some(utf16),
                length: Some(1),
                associated_value: Some(AssociatedValue::MentionAci(aci.clone())),
            });
        }
        utf16 += c.len_utf16() as u32;
    }
    ranges
}

/// Parses a direct conversation's service id.
pub fn service_id_of(conversation: &ConversationId) -> Option<ServiceId> {
    ServiceId::parse_from_service_id_string(conversation.as_direct()?)
}

/// A quote's attachment as sent (`Quote.QuotedAttachment`): its type and
/// name, so their client can say what was quoted. Desktop also sends a
/// thumbnail of a quoted photo; that's not sent here.
pub fn quoted_attachments(
    attachment: Option<&signal_store::QuotedAttachment>,
) -> Vec<presage::proto::data_message::quote::QuotedAttachment> {
    attachment
        .map(|a| presage::proto::data_message::quote::QuotedAttachment {
            content_type: Some(a.content_type.clone()).filter(|t| !t.is_empty()),
            file_name: a.file_name.clone(),
            thumbnail: None,
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests;

/// A call event from another of our devices about a 1:1 call.
#[derive(Debug, Clone, PartialEq)]
pub struct CallEventSync {
    /// Their ACI.
    pub peer: String,
    /// RingRTC's call id, its bits as an i64 (as call notices keep it).
    pub call_id: i64,
    pub timestamp: i64,
    pub video: bool,
    pub incoming: bool,
    /// `None`: deleted from the call history.
    pub outcome: Option<signal_store::CallOutcome>,
}

fn call_event(event: &sync_message::CallEvent) -> Option<CallEventSync> {
    use sync_message::call_event::{Direction, Event, Type};
    use signal_store::CallOutcome;
    let video = match event.r#type() {
        Type::AudioCall => false,
        Type::VideoCall => true,
        _ => return None,
    };
    let peer = parse_aci_with_fallback(event.conversation_id.as_deref(), None)?.service_id_string();
    let incoming = match event.direction() {
        Direction::Incoming => true,
        Direction::Outgoing => false,
        Direction::UnknownDirection => return None,
    };
    let outcome = match (event.event(), incoming) {
        (Event::Accepted, _) => Some(CallOutcome::Accepted),
        (Event::NotAccepted, true) => Some(CallOutcome::Declined),
        (Event::NotAccepted, false) => Some(CallOutcome::Unanswered),
        (Event::Delete, _) => None,
        _ => return None,
    };
    Some(CallEventSync {
        peer,
        call_id: event.call_id? as i64,
        timestamp: event.timestamp? as i64,
        video,
        incoming,
        outcome,
    })
}

/// A group call's event from another of our devices (Desktop's
/// `getCallEventForProto` with a `GROUP_CALL` type).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupCallEventSync {
    /// The group's identifier.
    pub group_id: Vec<u8>,
    pub call_id: i64,
    pub timestamp: i64,
    pub incoming: bool,
    /// They joined it; `false`: they deleted it from the call history.
    pub accepted: bool,
}

fn group_call_event(event: &sync_message::CallEvent) -> Option<GroupCallEventSync> {
    use sync_message::call_event::{Direction, Event, Type};
    if event.r#type() != Type::GroupCall {
        return None;
    }
    let incoming = match event.direction() {
        Direction::Incoming => true,
        Direction::Outgoing => false,
        Direction::UnknownDirection => return None,
    };
    // Desktop throws on the others for a group call.
    let accepted = match event.event() {
        Event::Accepted => true,
        Event::Delete => false,
        _ => return None,
    };
    Some(GroupCallEventSync {
        group_id: event.conversation_id.clone()?,
        call_id: event.call_id? as i64,
        timestamp: event.timestamp? as i64,
        incoming,
        accepted,
    })
}

/// What a call log event asks (Desktop's `CallLogEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallLogKind {
    Clear,
    MarkedAsRead,
    MarkedAsReadInConversation,
}

/// Another of our devices cleared or read its call history up to a call.
#[derive(Debug, Clone, PartialEq)]
pub struct CallLogEventSync {
    pub kind: CallLogKind,
    /// The call's chat: an ACI's 16 bytes or a group's identifier (or a
    /// call link's room id, which names nothing here).
    pub conversation: Option<Vec<u8>>,
    pub call_id: Option<i64>,
    /// The call's time.
    pub timestamp: i64,
    /// When they did it (the sync's own time).
    pub at: i64,
}

fn call_log_event(event: &sync_message::CallLogEvent, envelope_ts: i64) -> Option<CallLogEventSync> {
    use sync_message::call_log_event::Type;
    // An unknown (newer) type is refused, not read as the default `Clear`;
    // a missing one is proto2's default, `Clear`.
    let kind = match Type::try_from(event.r#type.unwrap_or_default()).ok()? {
        Type::Clear => CallLogKind::Clear,
        Type::MarkedAsRead => CallLogKind::MarkedAsRead,
        Type::MarkedAsReadInConversation => CallLogKind::MarkedAsReadInConversation,
        // Desktop doesn't do it either (`UNIMPLEMENTED_ClearInConversation`).
        Type::ClearInConversation => return None,
    };
    Some(CallLogEventSync {
        kind,
        conversation: event.conversation_id.clone(),
        call_id: event.call_id.map(|id| id as i64),
        timestamp: event.timestamp? as i64,
        at: envelope_ts,
    })
}
