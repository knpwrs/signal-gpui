//! Encrypted application database.
//!
//! Signal Desktop keeps a denormalized `conversations` / `messages` projection in
//! SQLCipher next to the protocol state. We do the same: presage owns protocol
//! state in its own SQLCipher file, and this crate owns everything the UI reads.
//! Both files are keyed with the same raw 256-bit key (`PRAGMA key = "x'…'"`).

use std::path::Path;

use sqlx::{
    AssertSqlSafe, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous},
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Schema(String),
}

/// Stable conversation key.
/// Direct conversations: `"d:<service id string>"` (ACI uuid).
/// Groups: `"g:<hex group master key>"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ConversationId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ConversationKind {
    Direct,
    Group,
    /// "Note to Self".
    NoteToSelf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub title: String,
    /// E.164 phone number, for direct conversations when known.
    pub phone: Option<String>,
    /// Index into the UI's avatar palette, stable per conversation.
    pub color: u8,
    /// A group's member ACIs (empty for direct chats).
    pub members: Vec<String>,
    pub last_message_preview: Option<String>,
    /// Unix millis of the last message; 0 if none.
    pub last_message_at: i64,
    pub unread_count: u32,
    pub pinned: bool,
    pub muted: bool,
    /// Unix millis; `i64::MAX` is forever, 0 or past is not muted.
    pub muted_until: i64,
    pub archived: bool,
    pub marked_unread: bool,
    pub blocked: bool,
    pub draft: Option<String>,
    /// Disappearing message timer in seconds; 0 = off.
    pub expire_timer: u32,
    /// Version of a 1:1 chat's timer (Signal's `expireTimerVersion`).
    pub expire_timer_version: u32,
    /// We may change the group's details (always true for 1:1 chats).
    pub can_edit_info: bool,
    /// We've left this group (or were removed).
    pub left: bool,
    /// A group's description.
    pub description: Option<String>,
    /// A group's admins (ACIs).
    pub admins: Vec<String>,
    /// Group member labels by ACI.
    pub member_labels: std::collections::BTreeMap<String, MemberLabel>,
    /// We may set our own member label.
    pub can_label: bool,
    /// Decrypted avatar image on disk, if one has been downloaded.
    pub avatar_path: Option<String>,
    /// Delivery status of the latest message when it is ours (for the
    /// conversation list's receipt icon); `None` when the latest is incoming.
    pub last_outgoing_status: Option<DeliveryStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum DeliveryStatus {
    Sending,
    Sent,
    Delivered,
    Read,
    Failed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reaction {
    pub emoji: String,
    /// Service id of the reactor; `None` = self.
    pub author: Option<String>,
    pub sent_at: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Quote {
    pub author: Option<String>,
    pub author_name: String,
    pub sent_at: i64,
    pub text: String,
    /// Local image shown beside the quote: the quoted message's photo or
    /// link preview image, when we have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    /// The quoted message's first attachment: what the quote says it is
    /// when it has no text ("Voice Message", "Photo").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<QuotedAttachment>,
}

/// What a quote knows of the quoted attachment (Signal's
/// `Quote.QuotedAttachment`, with the flags we can learn locally).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct QuotedAttachment {
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default)]
    pub voice: bool,
    #[serde(default)]
    pub gif: bool,
}

impl QuotedAttachment {
    pub fn of(attachment: &AttachmentMeta) -> Self {
        Self {
            content_type: attachment.content_type.clone(),
            file_name: attachment.file_name.clone(),
            voice: attachment.voice,
            gif: attachment.gif,
        }
    }

    /// What a quote of it says when there's no text, in Desktop's words
    /// (`Quote`'s `getTypeLabel`; any other file shows its name).
    pub fn label(&self) -> String {
        let kind = self.content_type.as_str();
        if self.gif || kind == "image/gif" {
            "GIF".into()
        } else if kind.starts_with("video/") {
            "Video".into()
        } else if kind.starts_with("image/") {
            "Photo".into()
        } else if kind.starts_with("audio/") && self.voice {
            "Voice Message".into()
        } else if kind.starts_with("audio/") {
            "Audio".into()
        } else {
            self.file_name.clone().unwrap_or_else(|| "File".into())
        }
    }
}

/// A link preview, as sent with the message (Signal's `Preview`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LinkPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Unix millis (the page's published or modified time).
    pub date: Option<i64>,
    pub image: Option<AttachmentMeta>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentMeta {
    pub content_type: String,
    pub file_name: Option<String>,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Path to the decrypted file once downloaded.
    pub local_path: Option<String>,
    /// A sticker: shown large, without a bubble.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sticker: bool,
    /// A looping, silent video sent as a GIF (Signal's GIF flag).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gif: bool,
    /// A voice message (Signal's voice-message flag): Desktop's audio player
    /// and auto-download.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub voice: bool,
    /// Where to download it when no stored message holds its pointer (an
    /// attachment from transferred history).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteAttachment>,
}

/// How a call went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// Answered (here or on another device).
    Accepted,
    /// Theirs, not answered.
    Missed,
    /// Theirs, declined.
    Declined,
    /// Ours, not answered.
    Unanswered,
}

/// What a read sync did (`AppDb::apply_read_sync`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSync {
    /// Messages in this chat were marked read.
    Read(ConversationId),
    /// The message was already read.
    AlreadyRead,
    /// No such message (yet: read syncs can arrive before their message).
    NotFound,
}

/// An attachment on Signal's transit CDN: enough to download and check it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteAttachment {
    pub cdn: u32,
    pub cdn_key: String,
    /// The 64-byte AES and HMAC key, hex.
    pub key: String,
    /// SHA-256 of the encrypted file, hex, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// SHA-256 of the decrypted file, hex, when that's what's known instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plaintext_hash: Option<String>,
}

/// A message as rendered in the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Row id; stable for the lifetime of the database.
    pub id: i64,
    pub conversation_id: ConversationId,
    /// Signal timestamp assigned by the sender (unix millis). With `author`
    /// this identifies a message across devices (used by reactions, quotes,
    /// receipts, deletes).
    pub sent_at: i64,
    pub received_at: i64,
    pub direction: Direction,
    /// Service id of the author; `None` for outgoing.
    pub author: Option<String>,
    /// Display name of the author at receive time.
    pub author_name: Option<String>,
    pub body: String,
    pub status: DeliveryStatus,
    pub reactions: Vec<Reaction>,
    pub quote: Option<Quote>,
    pub attachments: Vec<AttachmentMeta>,
    pub link_preview: Option<LinkPreview>,
    /// Remote-deleted ("This message was deleted").
    pub deleted: bool,
    pub edited: bool,
    pub expires_in: u32,
    /// When its disappearing countdown started (unix millis); `None` until
    /// sent or read.
    pub expire_start: Option<i64>,
    /// How many times it has been edited.
    pub edit_count: u32,
    /// A timeline notice rather than a message.
    pub notice: Option<Notice>,
    /// Where its @mentions are in `body`.
    pub mentions: Vec<MentionSpan>,
    /// Its formatting (bold, spoilers…), in `body`'s bytes.
    pub styles: Vec<StyleSpan>,
    /// A release note's button (the Signal chat's only).
    pub cta: Option<CallToAction>,
}

/// A release note's button: what it does (Desktop's `callToActionId`,
/// "donate") and what it says.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallToAction {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The Signal chat's made-up account (Desktop's `SIGNAL_ACI`,
/// `ts/types/SignalConversation.std.ts`): release notes appear from it, and
/// nothing is ever sent to it.
pub const SIGNAL_ACI: &str = "11111111-1111-4111-8111-111111111111";

/// A group member's label: a short text with an optional emoji.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemberLabel {
    pub emoji: Option<String>,
    pub text: String,
}

/// An @mention in a message body: `body[start..start + len]` is "@Name".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MentionSpan {
    pub start: usize,
    pub len: usize,
    /// Who is mentioned (service id).
    pub aci: String,
}

/// Signal's text styles (`BodyRange.Style`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStyle {
    Bold,
    Italic,
    Strikethrough,
    Monospace,
    /// Hidden until clicked; previews and notifications show `■■■■`.
    Spoiler,
}

/// Formatting over `body[start..start + len]` (bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StyleSpan {
    pub start: usize,
    pub len: usize,
    pub style: TextStyle,
}

/// What stands in for spoiler text where it can't be revealed (previews,
/// notifications, quotes), as in Desktop.
pub const SPOILER_REPLACEMENT: &str = "■■■■";

/// `body` with each spoiler's text replaced by [`SPOILER_REPLACEMENT`].
pub fn hide_spoilers(body: &str, styles: &[StyleSpan]) -> String {
    let mut spoilers: Vec<(usize, usize)> = styles
        .iter()
        .filter(|s| s.style == TextStyle::Spoiler)
        .map(|s| (s.start, s.start + s.len))
        .filter(|(start, end)| *end <= body.len() && body.is_char_boundary(*start) && body.is_char_boundary(*end))
        .collect();
    spoilers.sort();
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for (start, end) in spoilers {
        // Overlapping ones are one spoiler.
        if start < last {
            last = last.max(end);
            continue;
        }
        out.push_str(&body[last..start]);
        out.push_str(SPOILER_REPLACEMENT);
        last = end;
    }
    out.push_str(&body[last..]);
    out
}

/// Something that happened in a chat, shown centered in the timeline.
/// `by`: the person's service id, `None` for us.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    /// The disappearing-message timer changed (0 = off).
    Timer { by: Option<String>, seconds: u32 },
    /// A message was pinned (`author`/`sent_at` identify it).
    Pinned { by: Option<String>, author: Option<String>, sent_at: i64 },
    /// A group changed.
    Group { by: Option<String>, change: GroupNotice },
    /// Their identity key changed (Desktop's "Your safety number with …
    /// has changed"): `who` is their service id.
    SafetyNumberChanged { who: String },
    /// Desktop's "Alice started a group call" (`by`: `None` for us). `era`
    /// names the call (each run of it has its own), so a call is noted once.
    GroupCall {
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        era: Option<String>,
    },
    /// A call, in Desktop's call history words. `call_id` (RingRTC's id, its
    /// bits as an i64 so SQLite's JSON reads it exactly) ties our record of
    /// a call to our other devices'.
    Call {
        incoming: bool,
        video: bool,
        outcome: CallOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<i64>,
    },
}

/// One change to a group, as a notice shows it. Service ids are `None`
/// for us.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupNotice {
    Title { title: String },
    Description,
    Avatar,
    /// `member` joined or was added.
    Added { member: Option<String> },
    /// `member` left (when they're who changed it) or was removed.
    Removed { member: Option<String> },
    Admin { member: Option<String>, admin: bool },
    /// Who may edit the group's details: only admins, or all members.
    InfoAccess { admins_only: bool },
    /// Who may add members.
    MemberAccess { admins_only: bool },
    AnnouncementsOnly { on: bool },
    Ended,
}

impl Message {
    /// What a quote of this message shows beside it: its first downloaded
    /// image, else its link preview's image.
    pub fn thumbnail(&self) -> Option<String> {
        self.attachments
            .iter()
            .filter(|a| a.content_type.starts_with("image/"))
            .chain(self.link_preview.as_ref().and_then(|p| p.image.as_ref()))
            .find_map(|a| a.local_path.clone())
    }
}

/// Someone or a group the account knows, whether or not there's a
/// conversation yet.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryEntry {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub title: String,
    pub phone: Option<String>,
}

/// Per-chat state from the Storage Service.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatState {
    pub id: ConversationId,
    pub archived: bool,
    /// Unix millis; `i64::MAX` is forever, 0 is not muted.
    pub muted_until: i64,
    /// "Mark as unread": shows as unread until opened.
    pub marked_unread: bool,
    pub blocked: bool,
}

/// Input for [`AppDb::insert_message`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewMessage {
    pub conversation_id: ConversationId,
    pub sent_at: i64,
    pub received_at: i64,
    pub direction: Direction,
    pub author: Option<String>,
    pub author_name: Option<String>,
    pub body: String,
    pub status: DeliveryStatus,
    pub quote: Option<Quote>,
    pub attachments: Vec<AttachmentMeta>,
    pub link_preview: Option<LinkPreview>,
    pub expires_in: u32,
}

/// Upsert input for a conversation; `None` fields keep their stored value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationUpdate {
    pub kind: Option<ConversationKind>,
    pub title: Option<String>,
    pub phone: Option<String>,
    pub expire_timer: Option<u32>,
    pub color: Option<u8>,
    /// A group's member ACIs.
    pub members: Option<Vec<String>>,
    /// Whether we may change the group's details.
    pub can_edit_info: Option<bool>,
    /// A group's description (`Some("")` clears it) and admins.
    pub description: Option<String>,
    pub admins: Option<Vec<String>>,
    pub member_labels: Option<std::collections::BTreeMap<String, MemberLabel>>,
    pub can_label: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub message_id: i64,
    pub conversation_id: ConversationId,
    pub sent_at: i64,
    /// Body with matches wrapped in `\u{2}` … `\u{3}` markers.
    pub snippet: String,
    /// Who sent it: `None` for us.
    pub author: Option<String>,
    pub author_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmojiUsage {
    pub emoji: String,
    pub count: u32,
    pub last_used: i64,
}

impl ConversationId {
    pub fn direct(service_id: &str) -> Self {
        Self(format!("d:{service_id}"))
    }

    pub fn group(master_key: &[u8]) -> Self {
        let mut s = String::with_capacity(2 + master_key.len() * 2);
        s.push_str("g:");
        for b in master_key {
            s.push_str(&format!("{b:02x}"));
        }
        Self(s)
    }

    /// The Signal chat, where release notes appear.
    pub fn signal() -> Self {
        Self::direct(SIGNAL_ACI)
    }

    pub fn is_signal(&self) -> bool {
        self.as_direct() == Some(SIGNAL_ACI)
    }

    /// Service id string for direct conversations.
    pub fn as_direct(&self) -> Option<&str> {
        self.0.strip_prefix("d:")
    }

    /// Group master key bytes for group conversations.
    pub fn as_group(&self) -> Option<Vec<u8>> {
        let hex = self.0.strip_prefix("g:")?;
        if hex.len() % 2 != 0 {
            return None;
        }
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect()
    }

    pub fn is_group(&self) -> bool {
        self.0.starts_with("g:")
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Number of avatar colors the UI palette provides.
pub const AVATAR_COLORS: u8 = 12;

/// A conversation's avatar color before the backend knows better (a
/// group's color needs its group id, which only the backend can derive).
fn color_for(id: &str) -> u8 {
    match id.strip_prefix("d:") {
        Some(service_id) => avatar_color(service_id),
        None => avatar_color_of_bytes(id.as_bytes()),
    }
}

/// Signal's avatar color for an account (Desktop's `generateAvatarColor`):
/// the first byte of SHA-256 over the service id's binary form (16 bytes for
/// an ACI, `0x01` and 16 for a PNI), modulo the palette size.
pub fn avatar_color(service_id: &str) -> u8 {
    let (prefix, uuid) = match service_id.strip_prefix("PNI:") {
        Some(uuid) => (Some(1u8), uuid),
        None => (None, service_id),
    };
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    let bytes: Option<Vec<u8>> = (hex.len() == 32)
        .then(|| (0..32).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect())
        .flatten();
    match bytes {
        Some(bytes) => avatar_color_of_bytes(&prefix.into_iter().chain(bytes).collect::<Vec<_>>()),
        // Not a service id (e.g. a phone number): Desktop hashes the text.
        None => avatar_color_of_bytes(service_id.as_bytes()),
    }
}

/// The first byte of SHA-256 over `identifier`, modulo the palette (also
/// how Signal colors a group, from its group id).
pub fn avatar_color_of_bytes(identifier: &[u8]) -> u8 {
    use sha2::Digest as _;
    sha2::Sha256::digest(identifier)[0] % AVATAR_COLORS
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn kind_to_str(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Group => "group",
        ConversationKind::NoteToSelf => "note_to_self",
    }
}

fn kind_from_str(s: &str) -> ConversationKind {
    match s {
        "group" => ConversationKind::Group,
        "note_to_self" => ConversationKind::NoteToSelf,
        _ => ConversationKind::Direct,
    }
}

fn status_to_i64(status: DeliveryStatus) -> i64 {
    match status {
        DeliveryStatus::Sending => 0,
        DeliveryStatus::Sent => 1,
        DeliveryStatus::Delivered => 2,
        DeliveryStatus::Read => 3,
        DeliveryStatus::Failed => 4,
    }
}

fn status_from_i64(v: i64) -> DeliveryStatus {
    match v {
        0 => DeliveryStatus::Sending,
        1 => DeliveryStatus::Sent,
        2 => DeliveryStatus::Delivered,
        3 => DeliveryStatus::Read,
        _ => DeliveryStatus::Failed,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Turns free text into an FTS5 query: every token becomes a quoted prefix
/// term, so user input can never be parsed as FTS syntax.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

const CONVERSATION_COLUMNS: &str = "id, kind, title, phone, color, members, last_message_preview, \
     last_message_at, unread_count, pinned, (muted_until > CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)) AS muted, muted_until, archived, \
     marked_unread, blocked, draft, expire_timer, expire_timer_version, can_edit_info, left_group, description, admins, member_labels, can_label, avatar_path, \
     (SELECT CASE WHEN m.direction = 1 AND m.deleted = 0 THEN m.status END FROM messages m \
      WHERE m.conversation_id = conversations.id AND m.notice IS NULL ORDER BY m.sent_at DESC, m.id DESC LIMIT 1) \
      AS last_outgoing_status";

const MESSAGE_COLUMNS: &str = "id, conversation_id, sent_at, received_at, direction, author, \
     author_name, body, status, reactions, quote, attachments, link_preview, deleted, edited, expires_in, expire_start, edit_count, notice, mentions, styles, cta";

/// SQL matching a message whose own timestamp, or one of its edits', is
/// the bound parameter `{param}`.
fn sent_at_matches(param: &str) -> String {
    format!("(sent_at = {param} OR id IN (SELECT message_id FROM message_edits WHERE sent_at = {param}))")
}

/// How every transaction here starts (they all write); see `AppDb::write`.
const BEGIN_WRITE: &str = "BEGIN IMMEDIATE";

const INIT_VERSION: i64 = 20261013000000;
/// The last migration before the 16 development migrations were squashed
/// into one init.
const LAST_PRE_SQUASH: i64 = 20261028000000;
const PRE_SQUASH_COUNT: i64 = 16;
/// `message_edits`, a migration of its own until it was squashed into the
/// init too.
const MESSAGE_EDITS_VERSION: i64 = 20261029000000;

/// What the init gained after the first squash, for a database whose
/// schema matches that first init: added if missing.
const SINCE_FIRST_INIT: &str = "\
    CREATE TABLE IF NOT EXISTS message_edits ( \
        message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE, \
        sent_at    INTEGER NOT NULL, \
        PRIMARY KEY (message_id, sent_at) \
    ) STRICT; \
    CREATE INDEX IF NOT EXISTS message_edits_sent_at ON message_edits (sent_at);";

/// Lets a database migrated before a squash open under the squashed init.
///
/// Two histories have a schema matching the first init: every pre-squash
/// migration applied (plus a leftover `muted` column nothing reads), or the
/// first init itself, with or without the `message_edits` migration. Those
/// get what the init has gained since, and their history is rewritten to
/// the init alone; the data is kept. A partly migrated database is rebuilt
/// if it holds no chats, and refused otherwise.
async fn adopt_squashed_history(pool: &SqlitePool, migrator: &sqlx::migrate::Migrator) -> Result<()> {
    let has_history: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')")
            .fetch_one(pool)
            .await?;
    if !has_history {
        return Ok(());
    }
    let (count, max): (i64, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), MAX(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await?;
    let Some(max) = max else { return Ok(()) };
    // Every applied migration is one of ours, unchanged: nothing to adopt.
    let applied: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations WHERE success").fetch_all(pool).await?;
    if applied.iter().all(|(v, c)| migrator.iter().any(|m| m.version == *v && *m.checksum == **c)) {
        return Ok(());
    }
    let init = migrator
        .iter()
        .find(|m| m.version == INIT_VERSION)
        .expect("init migration is embedded");
    // The first init's last column, so its schema is all there.
    let first_init_schema: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pragma_table_info('conversations') WHERE name = 'can_label')")
            .fetch_one(pool)
            .await?;
    let pre_squash = count == PRE_SQUASH_COUNT && max == LAST_PRE_SQUASH;
    let first_init = applied.iter().any(|(v, _)| *v == INIT_VERSION)
        && applied.iter().all(|(v, _)| *v == INIT_VERSION || *v == MESSAGE_EDITS_VERSION);
    if first_init_schema && (pre_squash || first_init) {
        let mut tx = pool.begin_with(BEGIN_WRITE).await?;
        sqlx::raw_sql(SINCE_FIRST_INIT).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM _sqlx_migrations").execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?, ?, TRUE, ?, 0)",
        )
        .bind(init.version)
        .bind(&*init.description)
        .bind(&*init.checksum)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(());
    }
    let chats: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations").fetch_one(pool).await?;
    if chats > 0 {
        return Err(Error::Schema(format!(
            "app.db is from an older development build (migration {max}); unlink, or delete app.db and relink"
        )));
    }
    // Nothing worth keeping: drop everything and let the init rebuild it.
    let objects: Vec<(String, String)> = sqlx::query_as(
        "SELECT type, name FROM sqlite_master \
         WHERE type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE 'messages_fts_%'",
    )
    .fetch_all(pool)
    .await?;
    let mut conn = pool.acquire().await?;
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await?;
    for (kind, name) in objects {
        let sql = format!("DROP {} IF EXISTS \"{}\"", kind.to_uppercase(), name.replace('"', "\"\""));
        sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
    }
    sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
    Ok(())
}

fn conversation_from_row(row: &SqliteRow) -> Result<Conversation> {
    Ok(Conversation {
        id: ConversationId(row.try_get("id")?),
        kind: kind_from_str(row.try_get::<&str, _>("kind")?),
        title: row.try_get("title")?,
        phone: row.try_get("phone")?,
        color: row.try_get::<i64, _>("color")? as u8,
        members: row
            .try_get::<Option<String>, _>("members")?
            .map(|m| serde_json::from_str(&m))
            .transpose()?
            .unwrap_or_default(),
        last_message_preview: row.try_get("last_message_preview")?,
        last_message_at: row.try_get("last_message_at")?,
        unread_count: row.try_get::<i64, _>("unread_count")? as u32,
        pinned: row.try_get("pinned")?,
        muted: row.try_get("muted")?,
        muted_until: row.try_get("muted_until")?,
        archived: row.try_get("archived")?,
        marked_unread: row.try_get("marked_unread")?,
        blocked: row.try_get("blocked")?,
        draft: row.try_get("draft")?,
        expire_timer: row.try_get::<i64, _>("expire_timer")? as u32,
        expire_timer_version: row.try_get::<i64, _>("expire_timer_version")? as u32,
        can_edit_info: row.try_get("can_edit_info")?,
        left: row.try_get("left_group")?,
        description: row.try_get("description")?,
        admins: row
            .try_get::<Option<String>, _>("admins")?
            .map(|a| serde_json::from_str(&a))
            .transpose()?
            .unwrap_or_default(),
        member_labels: row
            .try_get::<Option<String>, _>("member_labels")?
            .map(|l| serde_json::from_str(&l))
            .transpose()?
            .unwrap_or_default(),
        can_label: row.try_get("can_label")?,
        avatar_path: row.try_get("avatar_path")?,
        last_outgoing_status: row.try_get::<Option<i64>, _>("last_outgoing_status")?.map(status_from_i64),
    })
}

fn message_from_row(row: &SqliteRow) -> Result<Message> {
    let quote: Option<String> = row.try_get("quote")?;
    let link_preview: Option<String> = row.try_get("link_preview")?;
    Ok(Message {
        id: row.try_get("id")?,
        conversation_id: ConversationId(row.try_get("conversation_id")?),
        sent_at: row.try_get("sent_at")?,
        received_at: row.try_get("received_at")?,
        direction: if row.try_get::<i64, _>("direction")? == 0 {
            Direction::Incoming
        } else {
            Direction::Outgoing
        },
        author: row.try_get("author")?,
        author_name: row.try_get("author_name")?,
        body: row.try_get("body")?,
        status: status_from_i64(row.try_get("status")?),
        reactions: serde_json::from_str(row.try_get("reactions")?)?,
        quote: quote.as_deref().map(serde_json::from_str).transpose()?,
        attachments: serde_json::from_str(row.try_get("attachments")?)?,
        link_preview: link_preview.as_deref().map(serde_json::from_str).transpose()?,
        deleted: row.try_get("deleted")?,
        edited: row.try_get("edited")?,
        expires_in: row.try_get::<i64, _>("expires_in")? as u32,
        expire_start: row.try_get("expire_start")?,
        edit_count: row.try_get::<i64, _>("edit_count")? as u32,
        notice: row.try_get::<Option<String>, _>("notice")?.map(|n| serde_json::from_str(&n)).transpose()?,
        mentions: row
            .try_get::<Option<String>, _>("mentions")?
            .map(|m| serde_json::from_str(&m))
            .transpose()?
            .unwrap_or_default(),
        styles: row
            .try_get::<Option<String>, _>("styles")?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default(),
        cta: row.try_get::<Option<String>, _>("cta")?.map(|c| serde_json::from_str(&c)).transpose()?,
    })
}

/// Handle to the encrypted application database. Cheap to clone.
/// All methods must be awaited on a Tokio runtime.
///
/// Writes queue for one connection (`writer`) rather than racing for
/// SQLite's lock: two connections writing at once would leave one waiting
/// out the busy timeout and then failing with "database is locked". Reads
/// use `pool`, whose connections can't write (`query_only`), so a write sent
/// there by mistake fails at once rather than racing.
#[derive(Clone, Debug)]
pub struct AppDb {
    pool: SqlitePool,
    writer: SqlitePool,
}

/// How long a write may wait for the ones queued ahead of it.
const WRITE_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl AppDb {
    /// Opens (creating if needed) the database at `path`, keyed with the raw
    /// 32-byte `key`, and runs migrations.
    pub async fn open(path: &Path, key: &[u8; 32]) -> Result<Self> {
        let options = keyed(SqliteConnectOptions::new().filename(path).create_if_missing(true), key);
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(WRITE_QUEUE_TIMEOUT)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options.clone().foreign_keys(true))
            .await?;
        let migrator = sqlx::migrate!("./migrations");
        adopt_squashed_history(&writer, &migrator).await?;
        migrator.run(&writer).await?;
        // (WAL mode lets these read while the writer writes.)
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options.foreign_keys(true).pragma("query_only", "ON"))
            .await?;
        Ok(Self { pool, writer })
    }

    /// In-memory database (tests).
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            .journal_mode(SqliteJournalMode::Memory);
        // Each in-memory connection is a separate database, so use exactly
        // one, for reads and writes alike.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(WRITE_QUEUE_TIMEOUT)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options.foreign_keys(true))
            .await?;
        let migrator = sqlx::migrate!("./migrations");
        adopt_squashed_history(&pool, &migrator).await?;
        migrator.run(&pool).await?;
        Ok(Self { writer: pool.clone(), pool })
    }

    /// Replaces all Storage Service contact names.
    pub async fn replace_contact_names(&self, names: &[(String, String)]) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM contact_names").execute(&mut *tx).await?;
        for (service_id, name) in names {
            sqlx::query("INSERT OR REPLACE INTO contact_names (service_id, name) VALUES (?, ?)")
                .bind(service_id)
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn contact_name(&self, service_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT name FROM contact_names WHERE service_id = ?")
            .bind(service_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Distinct incoming-message authors, for re-resolving their names.
    pub async fn message_authors(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT author_key FROM messages WHERE author_key <> '' \
             UNION SELECT DISTINCT json_extract(quote, '$.author') FROM messages \
             WHERE quote IS NOT NULL AND json_extract(quote, '$.author') IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Recomputes previews written by an older version of [`refresh_preview`],
    /// once.
    /// Recomputes direct chats' avatar colors with Signal's hash, once
    /// (groups get theirs from the backend, which can derive group ids).
    pub async fn upgrade_colors(&self) -> Result<()> {
        if self.setting("color_format").await?.as_deref() == Some("signal") {
            return Ok(());
        }
        let mut tx = self.write().await?;
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM conversations").fetch_all(&mut *tx).await?;
        for id in ids {
            sqlx::query("UPDATE conversations SET color = ? WHERE id = ?")
                .bind(i64::from(color_for(&id)))
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.set_setting("color_format", "signal").await
    }

    pub async fn upgrade_previews(&self) -> Result<()> {
        const PREVIEW_FORMAT: &str = "2"; // "You:" in groups, whole text
        if self.setting("preview_format").await?.as_deref() == Some(PREVIEW_FORMAT) {
            return Ok(());
        }
        self.refresh_all_previews().await?;
        self.set_setting("preview_format", PREVIEW_FORMAT).await
    }

    /// Recomputes every conversation's preview (after a change to how they're
    /// written, or to the names in them).
    pub async fn refresh_all_previews(&self) -> Result<()> {
        let mut tx = self.write().await?;
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM conversations").fetch_all(&mut *tx).await?;
        for id in ids {
            refresh_preview(&mut tx, &ConversationId(id)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Rewrites the stored author names (on messages and their quotes) for
    /// each `(service_id, name)`. Returns how many rows changed.
    pub async fn rename_authors(&self, names: &[(String, String)]) -> Result<u64> {
        // One transaction per person, so other writes queued behind this
        // one (a reaction arriving, say) wait for one rename, not all.
        let mut changed = 0;
        for (service_id, name) in names {
            let mut tx = self.write().await?;
            changed += sqlx::query(
                "UPDATE messages SET author_name = ?1 WHERE author_key = ?2 AND author_name IS NOT ?1",
            )
            .bind(name)
            .bind(service_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            changed += sqlx::query(
                "UPDATE messages SET quote = json_set(quote, '$.author_name', ?1) \
                 WHERE quote IS NOT NULL AND json_extract(quote, '$.author') = ?2 \
                   AND json_extract(quote, '$.author_name') IS NOT ?1",
            )
            .bind(name)
            .bind(service_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
        }
        if changed > 0 {
            // Group previews carry the sender's name.
            let mut tx = self.write().await?;
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM conversations WHERE kind = 'group' AND last_message_preview IS NOT NULL",
            )
            .fetch_all(&mut *tx)
            .await?;
            for id in ids {
                refresh_preview(&mut tx, &ConversationId(id)).await?;
            }
            tx.commit().await?;
        }
        Ok(changed)
    }

    pub async fn set_avatar(&self, id: &ConversationId, path: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE conversations SET avatar_path = ? WHERE id = ?")
            .bind(path)
            .bind(&id.0)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    pub async fn set_person_avatar(&self, service_id: &str, path: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO person_avatars (service_id, path) VALUES (?, ?)")
            .bind(service_id)
            .bind(path)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// `(service id, photo path)` for everyone with a downloaded photo.
    /// They have no profile photo (anymore).
    pub async fn remove_person_avatar(&self, service_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM person_avatars WHERE service_id = ?").bind(service_id).execute(&self.writer).await?;
        Ok(())
    }

    pub async fn person_avatars(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT service_id, path FROM person_avatars").fetch_all(&self.pool).await?)
    }

    /// Pinned first, then by `last_message_at` descending. Excludes archived.
    pub async fn conversations(&self) -> Result<Vec<Conversation>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE archived = 0 \
             ORDER BY pinned DESC, pin_order, last_message_at DESC, title"
        )))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(conversation_from_row).collect()
    }

    /// Archived conversations, most recent first.
    pub async fn archived_conversations(&self) -> Result<Vec<Conversation>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE archived = 1 \
             ORDER BY last_message_at DESC, title"
        )))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(conversation_from_row).collect()
    }

    pub async fn conversation(&self, id: &ConversationId) -> Result<Option<Conversation>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE id = ?"
        )))
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(conversation_from_row).transpose()
    }

    /// Creates the conversation if missing (title defaults to the id).
    pub async fn upsert_conversation(
        &self,
        id: &ConversationId,
        update: ConversationUpdate,
    ) -> Result<()> {
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, id).await?;
        sqlx::query(
            "UPDATE conversations SET \
                kind = COALESCE(?, kind), \
                title = COALESCE(?, title), \
                phone = COALESCE(?, phone), \
                expire_timer = COALESCE(?, expire_timer), \
                color = COALESCE(?, color), \
                members = COALESCE(?, members), \
                can_edit_info = COALESCE(?, can_edit_info), \
                description = CASE WHEN ? IS NULL THEN description ELSE NULLIF(?, '') END, \
                admins = COALESCE(?, admins), \
                member_labels = COALESCE(?, member_labels), \
                can_label = COALESCE(?, can_label) \
             WHERE id = ?",
        )
        .bind(update.kind.map(kind_to_str))
        .bind(update.title)
        .bind(update.phone)
        .bind(update.expire_timer.map(i64::from))
        .bind(update.color.map(i64::from))
        .bind(update.members.map(|m| serde_json::to_string(&m)).transpose()?)
        .bind(update.can_edit_info)
        .bind(update.description.as_deref())
        .bind(update.description.as_deref())
        .bind(update.admins.map(|a| serde_json::to_string(&a)).transpose()?)
        .bind(update.member_labels.map(|l| serde_json::to_string(&l)).transpose()?)
        .bind(update.can_label)
        .bind(&id.0)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_flag(&self, id: &ConversationId, column: &'static str, value: bool) -> Result<()> {
        sqlx::query(AssertSqlSafe(format!("UPDATE conversations SET {column} = ? WHERE id = ?")))
            .bind(value)
            .bind(&id.0)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    pub async fn set_pinned(&self, id: &ConversationId, pinned: bool) -> Result<()> {
        self.set_flag(id, "pinned", pinned).await
    }

    /// Pinned conversations, in order.
    pub async fn pinned_ids(&self) -> Result<Vec<ConversationId>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM conversations WHERE pinned = 1 ORDER BY pin_order")
            .fetch_all(&self.pool)
            .await?;
        Ok(ids.into_iter().map(ConversationId).collect())
    }

    /// Makes `ids` the pinned conversations, in this order, unpinning the rest.
    /// Pinned conversations are listed even before they have messages, so
    /// missing ones are created. Returns whether anything changed.
    pub async fn replace_pinned(&self, ids: &[ConversationId]) -> Result<bool> {
        let mut tx = self.write().await?;
        let before: Vec<String> =
            sqlx::query_scalar("SELECT id FROM conversations WHERE pinned = 1 ORDER BY pin_order")
                .fetch_all(&mut *tx)
                .await?;
        if before.iter().eq(ids.iter().map(|id| &id.0)) {
            return Ok(false);
        }
        sqlx::query("UPDATE conversations SET pinned = 0, pin_order = 0 WHERE pinned = 1")
            .execute(&mut *tx)
            .await?;
        for (order, id) in ids.iter().enumerate() {
            ensure_conversation(&mut tx, id).await?;
            sqlx::query("UPDATE conversations SET pinned = 1, pin_order = ? WHERE id = ?")
                .bind(order as i64)
                .bind(&id.0)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Mutes forever, or unmutes.
    pub async fn set_muted(&self, id: &ConversationId, muted: bool) -> Result<()> {
        self.set_muted_until(id, if muted { i64::MAX } else { 0 }).await
    }

    /// Mutes until `until` (unix millis; `i64::MAX` is forever, 0 unmutes).
    pub async fn set_muted_until(&self, id: &ConversationId, until: i64) -> Result<()> {
        sqlx::query("UPDATE conversations SET muted_until = ? WHERE id = ?")
            .bind(until)
            .bind(&id.0)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Applies chat state synced from the primary device. Only
    /// conversations that already exist are touched. Returns whether any
    /// changed.
    pub async fn apply_chat_states(&self, states: &[ChatState]) -> Result<bool> {
        let mut tx = self.write().await?;
        let mut changed = 0;
        for state in states {
            changed += sqlx::query(
                "UPDATE conversations SET archived = ?1, muted_until = ?2, marked_unread = ?3, blocked = ?4 \
                 WHERE id = ?5 AND (archived IS NOT ?1 OR muted_until IS NOT ?2 \
                   OR marked_unread IS NOT ?3 OR blocked IS NOT ?4)",
            )
            .bind(state.archived)
            .bind(state.muted_until)
            .bind(state.marked_unread)
            .bind(state.blocked)
            .bind(&state.id.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(changed > 0)
    }

    pub async fn set_archived(&self, id: &ConversationId, archived: bool) -> Result<()> {
        self.set_flag(id, "archived", archived).await
    }

    pub async fn set_marked_unread(&self, id: &ConversationId, marked: bool) -> Result<()> {
        self.set_flag(id, "marked_unread", marked).await
    }

    pub async fn set_blocked(&self, id: &ConversationId, blocked: bool) -> Result<()> {
        self.set_flag(id, "blocked", blocked).await
    }

    pub async fn set_draft(&self, id: &ConversationId, draft: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE conversations SET draft = ? WHERE id = ?")
            .bind(draft.filter(|d| !d.is_empty()))
            .bind(&id.0)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Zeroes the unread count; returns `(author, sent_at)` of incoming messages
    /// that were unread, so read receipts can be sent.
    pub async fn mark_read(&self, id: &ConversationId) -> Result<Vec<(String, i64)>> {
        let mut tx = self.write().await?;
        let rows = sqlx::query(
            "UPDATE messages SET is_read = 1, \
                expire_start = CASE WHEN expires_in > 0 THEN CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) END \
             WHERE conversation_id = ? AND is_read = 0 \
             RETURNING author, sent_at",
        )
        .bind(&id.0)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("UPDATE conversations SET unread_count = 0, marked_unread = 0 WHERE id = ?")
            .bind(&id.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(author) = row.try_get::<Option<String>, _>("author")? {
                out.push((author, row.try_get("sent_at")?));
            }
        }
        out.sort_by_key(|(_, ts)| *ts);
        Ok(out)
    }

    /// Marks one incoming message read (e.g. read on another device). Returns its
    /// conversation if the message was previously unread.
    pub async fn mark_message_read(&self, author: &str, sent_at: i64) -> Result<Option<ConversationId>> {
        let mut tx = self.write().await?;
        let conversation: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "UPDATE messages SET is_read = 1, \
                expire_start = CASE WHEN expires_in > 0 THEN CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) END \
             WHERE author_key = ?1 AND {} AND is_read = 0 RETURNING conversation_id",
            sent_at_matches("?2")
        )))
        .bind(author)
        .bind(sent_at)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(conversation) = &conversation {
            sqlx::query(
                "UPDATE conversations SET unread_count = \
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND is_read = 0) \
                 WHERE id = ?1",
            )
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(conversation.map(ConversationId))
    }

    /// A read sync from another device: marks that incoming message read and,
    /// as Desktop does, every older incoming message in its chat (a phone
    /// doesn't send one for each message, and some get lost).
    pub async fn apply_read_sync(&self, author: &str, sent_at: i64) -> Result<ReadSync> {
        let mut tx = self.write().await?;
        let target: Option<(String, i64, i64)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT conversation_id, sent_at, id FROM messages \
             WHERE author_key = ?1 AND direction = 0 AND {} ORDER BY sent_at = ?2 DESC LIMIT 1",
            sent_at_matches("?2")
        )))
        .bind(author)
        .bind(sent_at)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((conversation, target_sent_at, target_id)) = target else {
            return Ok(ReadSync::NotFound);
        };
        let read = sqlx::query(
            "UPDATE messages SET is_read = 1, \
                expire_start = CASE WHEN expires_in > 0 THEN CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) END \
             WHERE conversation_id = ?1 AND direction = 0 AND is_read = 0 \
               AND (sent_at < ?2 OR (sent_at = ?2 AND id <= ?3))",
        )
        .bind(&conversation)
        .bind(target_sent_at)
        .bind(target_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if read == 0 {
            return Ok(ReadSync::AlreadyRead);
        }
        sqlx::query(
            "UPDATE conversations SET unread_count = \
                (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND is_read = 0) \
             WHERE id = ?1",
        )
        .bind(&conversation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ReadSync::Read(ConversationId(conversation)))
    }

    /// Inserts a message, deduplicating on `(conversation_id, author, sent_at)`.
    /// Updates the conversation's preview/last_message_at and, for incoming
    /// messages, its unread count. Returns the row id (existing row on dedupe).
    pub async fn insert_message(&self, msg: &NewMessage) -> Result<i64> {
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, &msg.conversation_id).await?;
        let incoming = msg.direction == Direction::Incoming;
        let author_key = msg.author.as_deref().unwrap_or("");
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO messages (conversation_id, sent_at, received_at, direction, author, \
                author_key, author_name, body, status, quote, attachments, link_preview, expires_in, is_read, expire_start) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (conversation_id, author_key, sent_at) DO NOTHING \
             RETURNING id",
        )
        .bind(&msg.conversation_id.0)
        .bind(msg.sent_at)
        .bind(msg.received_at)
        .bind(if incoming { 0i64 } else { 1 })
        .bind(&msg.author)
        .bind(author_key)
        .bind(&msg.author_name)
        .bind(&msg.body)
        .bind(status_to_i64(msg.status))
        .bind(msg.quote.as_ref().map(serde_json::to_string).transpose()?)
        .bind(serde_json::to_string(&msg.attachments)?)
        .bind(msg.link_preview.as_ref().map(serde_json::to_string).transpose()?)
        .bind(i64::from(msg.expires_in))
        .bind(!incoming)
        // Ours count down from when they're sent; others' from when read.
        .bind((!incoming && msg.expires_in > 0).then_some(msg.received_at))
        .fetch_optional(&mut *tx)
        .await?;

        let id = match inserted {
            Some(id) => {
                if incoming {
                    // Like Signal, a new message brings an archived chat back
                    // unless it is muted.
                    sqlx::query(
                        "UPDATE conversations SET unread_count = unread_count + 1, \
                            archived = archived AND muted_until > \
                                CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) \
                         WHERE id = ?",
                    )
                    .bind(&msg.conversation_id.0)
                    .execute(&mut *tx)
                    .await?;
                }
                refresh_preview(&mut tx, &msg.conversation_id).await?;
                id
            }
            None => {
                sqlx::query_scalar(
                    "SELECT id FROM messages \
                     WHERE conversation_id = ? AND author_key = ? AND sent_at = ?",
                )
                .bind(&msg.conversation_id.0)
                .bind(author_key)
                .bind(msg.sent_at)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(id)
    }

    pub async fn message(&self, id: i64) -> Result<Option<Message>> {
        let row = sqlx::query(AssertSqlSafe(format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(message_from_row).transpose()
    }

    /// Finds a message by its cross-device identity. `author == None` = self.
    pub async fn find_message(
        &self,
        conversation: &ConversationId,
        author: Option<&str>,
        sent_at: i64,
    ) -> Result<Option<Message>> {
        // Its own timestamp wins over another message's edit.
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages \
             WHERE conversation_id = ?1 AND author_key = ?2 AND {} \
             ORDER BY sent_at = ?3 DESC LIMIT 1",
            sent_at_matches("?3")
        )))
        .bind(&conversation.0)
        .bind(author.unwrap_or(""))
        .bind(sent_at)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(message_from_row).transpose()
    }

    /// Finds a message by author and timestamp in any conversation. Used for
    /// content that doesn't name its conversation (e.g. receipts, sync reads).
    pub async fn find_message_any(&self, author: Option<&str>, sent_at: i64) -> Result<Option<Message>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE author_key = ?1 AND {} ORDER BY sent_at = ?2 DESC LIMIT 1",
            sent_at_matches("?2")
        )))
        .bind(author.unwrap_or(""))
        .bind(sent_at)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(message_from_row).transpose()
    }

    /// Most recent `limit` messages strictly older than `before` (a message id),
    /// returned oldest-first so they can be appended to a timeline directly.
    pub async fn messages(
        &self,
        conversation: &ConversationId,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let rows = match before {
            Some(before) => {
                sqlx::query(AssertSqlSafe(format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages \
                     WHERE conversation_id = ?1 \
                       AND (sent_at, id) < (SELECT sent_at, id FROM messages WHERE id = ?2) \
                     ORDER BY sent_at DESC, id DESC LIMIT ?3"
                )))
                .bind(&conversation.0)
                .bind(before)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(AssertSqlSafe(format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ? \
                     ORDER BY sent_at DESC, id DESC LIMIT ?"
                )))
                .bind(&conversation.0)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
        };
        let mut out = rows
            .iter()
            .map(message_from_row)
            .collect::<Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }

    /// Up to `before` messages before `message_id`, it, and up to `after`
    /// after it, oldest first (for jumping to a search result).
    pub async fn messages_around(
        &self,
        conversation: &ConversationId,
        message_id: i64,
        before: u32,
        after: u32,
    ) -> Result<Vec<Message>> {
        let Some(target) = self.message(message_id).await?.filter(|m| m.conversation_id == *conversation) else {
            return Ok(Vec::new());
        };
        let mut out = self.messages(conversation, Some(message_id), before).await?;
        out.push(target);
        out.extend(self.messages_after(conversation, message_id, after).await?);
        Ok(out)
    }

    /// A chat's messages with attachments or link previews, newest first
    /// (Desktop's "All media").
    pub async fn media_messages(&self, conversation: &ConversationId, limit: u32) -> Result<Vec<Message>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages \
             WHERE conversation_id = ? AND deleted = 0 AND notice IS NULL \
               AND (attachments != '[]' OR link_preview IS NOT NULL) \
             ORDER BY sent_at DESC, id DESC LIMIT ?"
        )))
        .bind(&conversation.0)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_from_row).collect()
    }

    /// Up to `limit` messages after `after_id`, oldest first.
    pub async fn messages_after(&self, conversation: &ConversationId, after_id: i64, limit: u32) -> Result<Vec<Message>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages \
             WHERE conversation_id = ?1 \
               AND (sent_at, id) > (SELECT sent_at, id FROM messages WHERE id = ?2) \
             ORDER BY sent_at, id LIMIT ?3"
        )))
        .bind(&conversation.0)
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_from_row).collect()
    }

    /// Outgoing messages: status only moves forward (Sending < Sent < Delivered < Read),
    /// except Failed which can be set from Sending.
    /// A failed message is being sent again.
    pub async fn mark_resending(&self, message_id: i64) -> Result<bool> {
        let done = sqlx::query("UPDATE messages SET status = ? WHERE id = ? AND status = ?")
            .bind(status_to_i64(DeliveryStatus::Sending))
            .bind(message_id)
            .bind(status_to_i64(DeliveryStatus::Failed))
            .execute(&self.writer)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn update_status(
        &self,
        conversation: &ConversationId,
        sent_at: i64,
        status: DeliveryStatus,
    ) -> Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE messages SET status = ?1 \
             WHERE conversation_id = ?2 AND author_key = '' AND {} \
               AND ((?1 != 4 AND status != 4 AND status < ?1) OR (?1 = 4 AND status = 0))",
            sent_at_matches("?3")
        )))
        .bind(status_to_i64(status))
        .bind(&conversation.0)
        .bind(sent_at)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Applies a delivery/read receipt to our outgoing message sent at `sent_at`,
    /// in whichever conversation it lives. Same forward-only rule as
    /// [`AppDb::update_status`]. Returns the ids of rows that changed.
    pub async fn update_status_by_sent_at(
        &self,
        sent_at: i64,
        status: DeliveryStatus,
    ) -> Result<Vec<(ConversationId, i64)>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "UPDATE messages SET status = ?1 \
             WHERE author_key = '' AND {} AND direction = 1 \
               AND ((?1 != 4 AND status != 4 AND status < ?1) OR (?1 = 4 AND status = 0)) \
             RETURNING conversation_id, id",
            sent_at_matches("?2")
        )))
        .bind(status_to_i64(status))
        .bind(sent_at)
        .fetch_all(&self.writer)
        .await?;
        rows.iter()
            .map(|r| Ok((ConversationId(r.try_get("conversation_id")?), r.try_get("id")?)))
            .collect()
    }

    /// Replaces any previous reaction by the same author on that message.
    pub async fn set_reaction(&self, message_id: i64, reaction: Reaction) -> Result<()> {
        self.modify_reactions(message_id, |list| {
            list.retain(|r| r.author != reaction.author);
            list.push(reaction);
        })
        .await
    }

    pub async fn remove_reaction(&self, message_id: i64, author: Option<&str>) -> Result<()> {
        self.modify_reactions(message_id, |list| {
            list.retain(|r| r.author.as_deref() != author);
        })
        .await
    }

    async fn modify_reactions(
        &self,
        message_id: i64,
        f: impl FnOnce(&mut Vec<Reaction>),
    ) -> Result<()> {
        let mut tx = self.write().await?;
        let Some(json) = sqlx::query_scalar::<_, String>("SELECT reactions FROM messages WHERE id = ?")
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(());
        };
        let mut list: Vec<Reaction> = serde_json::from_str(&json)?;
        f(&mut list);
        sqlx::query("UPDATE messages SET reactions = ? WHERE id = ?")
            .bind(serde_json::to_string(&list)?)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_deleted(&self, message_id: i64) -> Result<()> {
        self.update_and_refresh(
            message_id,
            "UPDATE messages SET deleted = 1, body = '', attachments = '[]', reactions = '[]', \
             quote = NULL, link_preview = NULL, mentions = NULL, styles = NULL WHERE id = ? RETURNING conversation_id",
            None,
        )
        .await
    }

    /// Applies an edit sent at `edit_sent_at`, which then names the message
    /// too (reactions to the edited message use it).
    /// A deleted message ignores it.
    pub async fn edit_message(&self, message_id: i64, body: &str, edit_sent_at: i64) -> Result<()> {
        let mut tx = self.write().await?;
        let conversation: Option<String> = sqlx::query_scalar(
            // The old formatting doesn't fit the new text; callers set the edit's.
            "UPDATE messages SET body = ?2, styles = NULL, edited = 1, edit_count = edit_count + 1 WHERE id = ?1 AND deleted = 0 \
             RETURNING conversation_id",
        )
        .bind(message_id)
        .bind(body)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(conversation) = conversation {
            sqlx::query("INSERT OR IGNORE INTO message_edits (message_id, sent_at) VALUES (?, ?)")
                .bind(message_id)
                .bind(edit_sent_at)
                .execute(&mut *tx)
                .await?;
            refresh_preview(&mut tx, &ConversationId(conversation)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Removes a message entirely ("delete for me"), keeping the
    /// conversation's preview and unread count right.
    pub async fn remove_message(&self, message_id: i64) -> Result<Option<ConversationId>> {
        let mut tx = self.write().await?;
        let conversation: Option<String> =
            sqlx::query_scalar("DELETE FROM messages WHERE id = ? RETURNING conversation_id")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(conversation) = &conversation {
            sqlx::query(
                "UPDATE conversations SET unread_count = \
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND is_read = 0) \
                 WHERE id = ?1",
            )
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
            refresh_preview(&mut tx, &ConversationId(conversation.clone())).await?;
            sqlx::query("DELETE FROM pinned_messages WHERE message_id = ?").bind(message_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(conversation.map(ConversationId))
    }

    /// Removes `through` and every message received before it in its chat
    /// (a chat deleted on another device, up to what that device had).
    /// Returns how many messages are left.
    pub async fn remove_messages_through(&self, conversation: &ConversationId, through: i64) -> Result<i64> {
        let mut tx = self.write().await?;
        sqlx::query(
            "DELETE FROM messages WHERE conversation_id = ?1 \
               AND (received_at, id) <= (SELECT received_at, id FROM messages WHERE id = ?2 AND conversation_id = ?1)",
        )
        .bind(&conversation.0)
        .bind(through)
        .execute(&mut *tx)
        .await?;
        // Their pins go with them (they'd count toward the limit).
        sqlx::query(
            "DELETE FROM pinned_messages WHERE conversation_id = ?1 \
             AND message_id NOT IN (SELECT id FROM messages WHERE conversation_id = ?1)",
        )
        .bind(&conversation.0)
        .execute(&mut *tx)
        .await?;
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(&conversation.0)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE conversations SET unread_count = \
                (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND is_read = 0) \
             WHERE id = ?1",
        )
        .bind(&conversation.0)
        .execute(&mut *tx)
        .await?;
        refresh_preview(&mut tx, conversation).await?;
        tx.commit().await?;
        Ok(left)
    }

    /// Disappeared messages: `(id, conversation)` of each whose countdown
    /// has run out by `now`.
    pub async fn expired_messages(&self, now: i64) -> Result<Vec<(i64, ConversationId)>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, conversation_id FROM messages \
             WHERE expire_start IS NOT NULL AND expire_start + expires_in * 1000 <= ?",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id, c)| (id, ConversationId(c))).collect())
    }

    /// When the next message disappears (unix millis), if any is counting down.
    pub async fn next_expiry(&self) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT MIN(expire_start + expires_in * 1000) FROM messages WHERE expire_start IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Sets a chat's disappearing timer and its version.
    pub async fn set_expire_timer(&self, id: &ConversationId, seconds: u32, version: u32) -> Result<()> {
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, id).await?;
        sqlx::query("UPDATE conversations SET expire_timer = ?, expire_timer_version = ? WHERE id = ?")
            .bind(i64::from(seconds))
            .bind(i64::from(version))
            .bind(&id.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Best-known display names for people (Storage Service name, 1:1 chat
    /// title, or the name on their latest message), for @mentions.
    pub async fn display_names(&self, service_ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(service_ids.len());
        for sid in service_ids {
            let name: Option<String> = sqlx::query_scalar(
                "SELECT name FROM ( \
                    SELECT name, 1 AS rank FROM contact_names WHERE service_id = ?1 \
                    UNION ALL SELECT title, 2 FROM conversations WHERE id = 'd:' || ?1 AND title != '' \
                    UNION ALL SELECT author_name, 3 FROM (SELECT author_name FROM messages \
                        WHERE author = ?1 AND author_name IS NOT NULL ORDER BY sent_at DESC LIMIT 1) \
                 ) ORDER BY rank LIMIT 1",
            )
            .bind(sid)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(name) = name {
                out.push((sid.clone(), name));
            }
        }
        Ok(out)
    }

    /// Records where a message's @mentions are.
    pub async fn set_mentions(&self, message_id: i64, mentions: &[MentionSpan]) -> Result<()> {
        let json = (!mentions.is_empty()).then(|| serde_json::to_string(mentions)).transpose()?;
        sqlx::query("UPDATE messages SET mentions = ? WHERE id = ?").bind(json).bind(message_id).execute(&self.writer).await?;
        Ok(())
    }

    /// Records a release note's button.
    pub async fn set_cta(&self, message_id: i64, cta: &CallToAction) -> Result<()> {
        sqlx::query("UPDATE messages SET cta = ? WHERE id = ?")
            .bind(serde_json::to_string(cta)?)
            .bind(message_id)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Records a message's formatting. Its chat's preview follows, as it
    /// hides spoilers.
    pub async fn set_styles(&self, message_id: i64, styles: &[StyleSpan]) -> Result<()> {
        let json = (!styles.is_empty()).then(|| serde_json::to_string(styles)).transpose()?;
        let mut tx = self.write().await?;
        let conversation: Option<String> =
            sqlx::query_scalar("UPDATE messages SET styles = ? WHERE id = ? RETURNING conversation_id")
                .bind(json)
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(conversation) = conversation {
            refresh_preview(&mut tx, &ConversationId(conversation)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Adds a timeline notice at `at` (unix millis). It's already read and
    /// doesn't become the chat's preview. Repeats of the same event are
    /// ignored. Returns its row id when new.
    pub async fn insert_notice(&self, conversation: &ConversationId, at: i64, notice: &Notice) -> Result<Option<i64>> {
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, conversation).await?;
        let id = sqlx::query_scalar(
            "INSERT INTO messages (conversation_id, sent_at, received_at, direction, author_key, body, status, \
                attachments, is_read, notice) \
             VALUES (?1, ?2, ?2, 0, 'notice', '', ?3, '[]', 1, ?4) \
             ON CONFLICT (conversation_id, author_key, sent_at) DO NOTHING RETURNING id",
        )
        .bind(&conversation.0)
        .bind(at)
        .bind(status_to_i64(DeliveryStatus::Read))
        .bind(serde_json::to_string(notice)?)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Records a call's notice, or updates the one for the same call (seen
    /// here and on another device, say). Returns its row id.
    pub async fn record_call(&self, conversation: &ConversationId, at: i64, notice: &Notice) -> Result<Option<i64>> {
        if let Notice::Call { call_id: Some(call_id), .. } = notice {
            let existing: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM messages WHERE conversation_id = ?1 AND author_key = 'notice' \
                   AND json_extract(notice, '$.type') = 'call' AND json_extract(notice, '$.call_id') = ?2",
            )
            .bind(&conversation.0)
            .bind(call_id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(id) = existing {
                sqlx::query("UPDATE messages SET notice = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(serde_json::to_string(notice)?)
                    .execute(&self.writer)
                    .await?;
                return Ok(Some(id));
            }
        }
        // Notices dedupe by time: step past another in the same millisecond.
        for offset in 0..10 {
            if let Some(id) = self.insert_notice(conversation, at + offset, notice).await? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Notes a group call ("… started a group call") unless its era is
    /// noted already. `None`: it was.
    pub async fn record_group_call(&self, conversation: &ConversationId, at: i64, notice: &Notice) -> Result<Option<i64>> {
        if let Notice::GroupCall { era: Some(era), .. } = notice {
            let existing: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM messages WHERE conversation_id = ?1 AND author_key = 'notice' \
                   AND json_extract(notice, '$.type') = 'group_call' AND json_extract(notice, '$.era') = ?2",
            )
            .bind(&conversation.0)
            .bind(era)
            .fetch_optional(&self.pool)
            .await?;
            if existing.is_some() {
                return Ok(None);
            }
        }
        self.insert_notice(conversation, at, notice).await
    }

    /// Removes a call's notice (deleted from the call history elsewhere).
    pub async fn remove_call(&self, conversation: &ConversationId, call_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "DELETE FROM messages WHERE conversation_id = ?1 AND author_key = 'notice' \
               AND json_extract(notice, '$.type') = 'call' AND json_extract(notice, '$.call_id') = ?2 RETURNING id",
        )
        .bind(&conversation.0)
        .bind(call_id)
        .fetch_optional(&self.writer)
        .await?)
    }

    pub async fn set_left(&self, id: &ConversationId, left: bool) -> Result<()> {
        self.set_flag(id, "left_group", left).await
    }

    pub async fn set_can_edit_info(&self, id: &ConversationId, can: bool) -> Result<()> {
        self.set_flag(id, "can_edit_info", can).await
    }

    /// Pins a message; past `limit` pins, the oldest goes (Desktop replaces
    /// it). Returns whether anything changed.
    pub async fn pin_message(
        &self,
        conversation: &ConversationId,
        message_id: i64,
        pinned_at: i64,
        expires_at: Option<i64>,
        limit: usize,
    ) -> Result<bool> {
        let mut tx = self.write().await?;
        let changed = sqlx::query(
            "INSERT INTO pinned_messages (conversation_id, message_id, pinned_at, expires_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (conversation_id, message_id) DO UPDATE SET pinned_at = excluded.pinned_at, expires_at = excluded.expires_at",
        )
        .bind(&conversation.0)
        .bind(message_id)
        .bind(pinned_at)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // Expired pins don't count toward the limit.
        sqlx::query("DELETE FROM pinned_messages WHERE expires_at IS NOT NULL AND expires_at <= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM pinned_messages WHERE conversation_id = ?1 AND message_id NOT IN \
                (SELECT message_id FROM pinned_messages WHERE conversation_id = ?1 ORDER BY pinned_at DESC LIMIT ?2)",
        )
        .bind(&conversation.0)
        .bind(limit as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(changed > 0)
    }

    pub async fn unpin_message(&self, conversation: &ConversationId, message_id: i64) -> Result<bool> {
        let done = sqlx::query("DELETE FROM pinned_messages WHERE conversation_id = ? AND message_id = ?")
            .bind(&conversation.0)
            .bind(message_id)
            .execute(&self.writer)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// A chat's pinned messages that haven't expired, newest pin first.
    pub async fn pinned_messages(&self, conversation: &ConversationId) -> Result<Vec<Message>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE id IN \
                (SELECT message_id FROM pinned_messages p WHERE p.conversation_id = ?1 \
                   AND (p.expires_at IS NULL OR p.expires_at > CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER))) \
             ORDER BY (SELECT pinned_at FROM pinned_messages p WHERE p.message_id = messages.id AND p.conversation_id = ?1) DESC"
        )))
        .bind(&conversation.0)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_from_row).collect()
    }

    /// Removes a conversation and all its messages ("delete chat").
    pub async fn remove_conversation(&self, id: &ConversationId) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?").bind(&id.0).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM pinned_messages WHERE conversation_id = ?").bind(&id.0).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM conversations WHERE id = ?").bind(&id.0).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_and_refresh(&self, message_id: i64, sql: &'static str, body: Option<&str>) -> Result<()> {
        let mut tx = self.write().await?;
        let mut q = sqlx::query_scalar::<_, String>(sql).bind(message_id);
        if let Some(body) = body {
            q = q.bind(body);
        }
        if let Some(conversation) = q.fetch_optional(&mut *tx).await? {
            refresh_preview(&mut tx, &ConversationId(conversation)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_attachment_path(&self, message_id: i64, index: usize, path: &str) -> Result<()> {
        let mut tx = self.write().await?;
        let Some(json) =
            sqlx::query_scalar::<_, String>("SELECT attachments FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(());
        };
        let mut list: Vec<AttachmentMeta> = serde_json::from_str(&json)?;
        if let Some(att) = list.get_mut(index) {
            att.local_path = Some(path.to_owned());
            sqlx::query("UPDATE messages SET attachments = ? WHERE id = ?")
                .bind(serde_json::to_string(&list)?)
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Replaces the directory with `entries`.
    pub async fn replace_directory(&self, entries: &[DirectoryEntry]) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM directory").execute(&mut *tx).await?;
        for entry in entries {
            sqlx::query("INSERT OR REPLACE INTO directory (id, kind, title, phone) VALUES (?, ?, ?, ?)")
                .bind(&entry.id.0)
                .bind(kind_to_str(entry.kind))
                .bind(&entry.title)
                .bind(&entry.phone)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Directory entries matching `query` (title words or phone digits) that
    /// have no conversation yet, by title.
    /// Everyone you could add to a group: your 1:1 chats (not blocked) and
    /// the people in your contacts, by name.
    pub async fn people(&self) -> Result<Vec<DirectoryEntry>> {
        let rows = sqlx::query(
            "SELECT id, title, phone FROM conversations WHERE kind = 'direct' AND blocked = 0 AND title != '' AND id != ?1 \
             UNION SELECT id, title, phone FROM directory WHERE kind = 'direct' \
                AND id NOT IN (SELECT id FROM conversations) \
             ORDER BY title COLLATE NOCASE",
        )
        // The Signal chat isn't someone to add or message.
        .bind(&ConversationId::signal().0)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(DirectoryEntry {
                    id: ConversationId(row.try_get("id")?),
                    kind: ConversationKind::Direct,
                    title: row.try_get("title")?,
                    phone: row.try_get("phone")?,
                })
            })
            .collect()
    }

    pub async fn search_directory(&self, query: &str, limit: u32) -> Result<Vec<DirectoryEntry>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let digits: String = query.chars().filter(char::is_ascii_digit).collect();
        let rows = sqlx::query(
            "SELECT id, kind, title, phone FROM directory \
             WHERE id NOT IN (SELECT id FROM conversations) \
               AND (title LIKE '%' || ?1 || '%' ESCAPE '\\' \
                    OR (?2 <> '' AND replace(phone, '+', '') LIKE '%' || ?2 || '%')) \
             ORDER BY title COLLATE NOCASE LIMIT ?3",
        )
        .bind(&escaped)
        .bind(&digits)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(DirectoryEntry {
                    id: ConversationId(row.try_get("id")?),
                    kind: kind_from_str(row.try_get("kind")?),
                    title: row.try_get("title")?,
                    phone: row.try_get("phone")?,
                })
            })
            .collect()
    }

    /// Records where a message's link preview image was saved.
    pub async fn set_preview_image_path(&self, message_id: i64, path: &str) -> Result<()> {
        let mut tx = self.write().await?;
        let json: Option<String> = sqlx::query_scalar("SELECT link_preview FROM messages WHERE id = ?")
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        let Some(json) = json else { return Ok(()) };
        let mut preview: LinkPreview = serde_json::from_str(&json)?;
        if let Some(image) = &mut preview.image {
            image.local_path = Some(path.to_owned());
            sqlx::query("UPDATE messages SET link_preview = ? WHERE id = ?")
                .bind(serde_json::to_string(&preview)?)
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Full-text search across message bodies (FTS5), newest first.
    pub async fn search_messages(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.search(query, None, limit).await
    }

    /// Search within one conversation (Desktop's "Search in chat").
    pub async fn search_conversation(&self, conversation: &ConversationId, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.search(query, Some(conversation), limit).await
    }

    async fn search(&self, query: &str, conversation: Option<&ConversationId>, limit: u32) -> Result<Vec<SearchHit>> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query(
            "SELECT m.id, m.conversation_id, m.sent_at, m.author, m.author_name, \
                snippet(messages_fts, 0, char(2), char(3), '…', 12) AS snippet \
             FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid \
             WHERE messages_fts MATCH ?1 AND m.deleted = 0 AND (?2 IS NULL OR m.conversation_id = ?2) \
             ORDER BY m.sent_at DESC LIMIT ?3",
        )
        .bind(fts)
        .bind(conversation.map(|c| c.0.as_str()))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(SearchHit {
                    message_id: r.try_get("id")?,
                    conversation_id: ConversationId(r.try_get("conversation_id")?),
                    sent_at: r.try_get("sent_at")?,
                    snippet: r.try_get("snippet")?,
                    author: r.try_get("author")?,
                    author_name: r.try_get("author_name")?,
                })
            })
            .collect()
    }

    pub async fn record_emoji_use(&self, emoji: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO emoji_usage (emoji, count, last_used) VALUES (?1, 1, ?2) \
             ON CONFLICT (emoji) DO UPDATE SET count = count + 1, last_used = ?2",
        )
        .bind(emoji)
        .bind(now_millis())
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Most recently used first.
    pub async fn emoji_usage(&self) -> Result<Vec<EmojiUsage>> {
        let rows = sqlx::query("SELECT emoji, count, last_used FROM emoji_usage ORDER BY last_used DESC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok(EmojiUsage {
                    emoji: r.try_get("emoji")?,
                    count: r.try_get::<i64, _>("count")? as u32,
                    last_used: r.try_get("last_used")?,
                })
            })
            .collect()
    }

    /// Small key/value table for UI settings.
    pub async fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Deletes everything about the account (used when unlinking).
    pub async fn clear(&self) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM message_edits").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM messages").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM conversations").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM settings").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM contact_names").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM directory").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM person_avatars").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM pinned_messages").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM emoji_usage").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A transaction that holds the write lock from the start. A deferred
    /// one reads first and then upgrades to write, and when another
    /// connection has committed in between, SQLite fails it with "database
    /// is locked" at once instead of waiting out the busy timeout.
    async fn write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.writer.begin_with(BEGIN_WRITE).await?)
    }

    /// Closes the pool, waiting for connections to finish.
    pub async fn close(&self) {
        self.pool.close().await;
        self.writer.close().await;
    }
}

/// Applies the SQLCipher raw key. sqlx issues `key` before every other pragma.
pub fn keyed(options: SqliteConnectOptions, key: &[u8; 32]) -> SqliteConnectOptions {
    options
        .pragma("key", format!("\"x'{}'\"", hex(key)))
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
}

async fn ensure_conversation(
    conn: &mut sqlx::SqliteConnection,
    id: &ConversationId,
) -> Result<()> {
    let kind = if id.is_group() {
        ConversationKind::Group
    } else {
        ConversationKind::Direct
    };
    let title = match (id.as_direct(), id.is_group()) {
        (Some(sid), _) => sid.get(..8).unwrap_or(sid).to_owned(),
        (None, true) => "Group".to_owned(),
        _ => id.0.clone(),
    };
    sqlx::query("INSERT OR IGNORE INTO conversations (id, kind, title, color) VALUES (?, ?, ?, ?)")
        .bind(&id.0)
        .bind(kind_to_str(kind))
        .bind(title)
        .bind(i64::from(color_for(&id.0)))
        .execute(conn)
        .await?;
    Ok(())
}

/// Recomputes the conversation's preview from its newest message.
async fn refresh_preview(conn: &mut sqlx::SqliteConnection, id: &ConversationId) -> Result<()> {
    let row = sqlx::query(
        "SELECT m.sent_at, m.body, m.styles, m.deleted, m.attachments, m.direction, m.author_name, c.kind \
         FROM messages m JOIN conversations c ON c.id = m.conversation_id \
         WHERE m.conversation_id = ? AND m.notice IS NULL ORDER BY m.sent_at DESC, m.id DESC LIMIT 1",
    )
    .bind(&id.0)
    .fetch_optional(&mut *conn)
    .await?;
    let (preview, at) = match row {
        None => (None, 0i64),
        Some(row) => {
            let styles: Vec<StyleSpan> =
                row.try_get::<Option<String>, _>("styles")?.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
            let body = hide_spoilers(&row.try_get::<String, _>("body")?, &styles);
            let text = if row.try_get::<bool, _>("deleted")? {
                "This message was deleted".to_owned()
            } else if !body.is_empty() {
                // Line breaks collapse to spaces; the list clamps to two lines.
                body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
            } else {
                let atts: Vec<AttachmentMeta> = serde_json::from_str(row.try_get("attachments")?)?;
                match atts.first() {
                    Some(a) if a.content_type.starts_with("image/") => "📷 Photo".to_owned(),
                    Some(a) if a.content_type.starts_with("video/") => "🎥 Video".to_owned(),
                    Some(a) if a.content_type.starts_with("audio/") => "🎤 Voice message".to_owned(),
                    Some(_) => "📎 File".to_owned(),
                    None => String::new(),
                }
            };
            let group = row.try_get::<&str, _>("kind")? == "group";
            let incoming = row.try_get::<i64, _>("direction")? == 0;
            let author: Option<String> = row.try_get("author_name")?;
            // Like Signal Desktop: groups say who sent it ("You:" or a first
            // name); one-to-one chats don't.
            let text = match author {
                _ if group && !incoming => format!("You: {text}"),
                Some(name) if group && !name.is_empty() => {
                    let first = name.split_whitespace().next().unwrap_or(&name);
                    format!("{first}: {text}")
                }
                _ => text,
            };
            (Some(text), row.try_get::<i64, _>("sent_at")?)
        }
    };
    sqlx::query("UPDATE conversations SET last_message_preview = ?, last_message_at = ? WHERE id = ?")
        .bind(preview)
        .bind(at)
        .bind(&id.0)
        .execute(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
