//! Encrypted application database.
//!
//! Signal Desktop keeps a denormalized `conversations` / `messages` projection in
//! SQLCipher next to the protocol state. We do the same: presage owns protocol
//! state in its own SQLCipher file, and this crate owns everything the UI reads.
//! Both files are keyed with the same raw 256-bit key (`PRAGMA key = "x'…'"`).

use std::path::Path;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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
    pub last_message_preview: Option<String>,
    /// Unix millis of the last message; 0 if none.
    pub last_message_at: i64,
    pub unread_count: u32,
    pub pinned: bool,
    pub muted: bool,
    pub archived: bool,
    pub draft: Option<String>,
    /// Disappearing message timer in seconds; 0 = off.
    pub expire_timer: u32,
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
    /// Remote-deleted ("This message was deleted").
    pub deleted: bool,
    pub edited: bool,
    pub expires_in: u32,
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
    pub expires_in: u32,
}

/// Upsert input for a conversation; `None` fields keep their stored value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationUpdate {
    pub kind: Option<ConversationKind>,
    pub title: Option<String>,
    pub phone: Option<String>,
    pub expire_timer: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub message_id: i64,
    pub conversation_id: ConversationId,
    pub sent_at: i64,
    /// Body with matches wrapped in `\u{2}` … `\u{3}` markers.
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmojiUsage {
    pub emoji: String,
    pub count: u32,
    pub last_used: i64,
}

/// Handle to the encrypted application database. Cheap to clone.
/// All methods must be awaited on a Tokio runtime.
#[derive(Clone, Debug)]
pub struct AppDb {
    pool: sqlx::SqlitePool,
}

impl AppDb {
    /// Opens (creating if needed) the database at `path`, keyed with the raw
    /// 32-byte `key`, and runs migrations.
    pub async fn open(path: &Path, key: &[u8; 32]) -> Result<Self> { todo!() }

    /// In-memory database (tests).
    pub async fn open_in_memory() -> Result<Self> { todo!() }

    /// Pinned first, then by `last_message_at` descending. Excludes archived.
    pub async fn conversations(&self) -> Result<Vec<Conversation>> { todo!() }
    pub async fn conversation(&self, id: &ConversationId) -> Result<Option<Conversation>> { todo!() }
    /// Creates the conversation if missing (title defaults to the id).
    pub async fn upsert_conversation(&self, id: &ConversationId, update: ConversationUpdate) -> Result<()> { todo!() }
    pub async fn set_pinned(&self, id: &ConversationId, pinned: bool) -> Result<()> { todo!() }
    pub async fn set_muted(&self, id: &ConversationId, muted: bool) -> Result<()> { todo!() }
    pub async fn set_archived(&self, id: &ConversationId, archived: bool) -> Result<()> { todo!() }
    pub async fn set_draft(&self, id: &ConversationId, draft: Option<&str>) -> Result<()> { todo!() }
    /// Zeroes the unread count; returns `(author, sent_at)` of incoming messages
    /// that were unread, so read receipts can be sent.
    pub async fn mark_read(&self, id: &ConversationId) -> Result<Vec<(String, i64)>> { todo!() }

    /// Inserts a message, deduplicating on `(conversation_id, author, sent_at)`.
    /// Updates the conversation's preview/last_message_at and, for incoming
    /// messages, its unread count. Returns the row id (existing row on dedupe).
    pub async fn insert_message(&self, msg: &NewMessage) -> Result<i64> { todo!() }
    pub async fn message(&self, id: i64) -> Result<Option<Message>> { todo!() }
    /// Finds a message by its cross-device identity. `author == None` = self.
    pub async fn find_message(&self, conversation: &ConversationId, author: Option<&str>, sent_at: i64) -> Result<Option<Message>> { todo!() }
    /// Most recent `limit` messages strictly older than `before` (a message id),
    /// returned oldest-first so they can be appended to a timeline directly.
    pub async fn messages(&self, conversation: &ConversationId, before: Option<i64>, limit: u32) -> Result<Vec<Message>> { todo!() }
    /// Outgoing messages: status only moves forward (Sending < Sent < Delivered < Read),
    /// except Failed which can be set from Sending.
    pub async fn update_status(&self, conversation: &ConversationId, sent_at: i64, status: DeliveryStatus) -> Result<()> { todo!() }
    /// Replaces any previous reaction by the same author on that message.
    pub async fn set_reaction(&self, message_id: i64, reaction: Reaction) -> Result<()> { todo!() }
    pub async fn remove_reaction(&self, message_id: i64, author: Option<&str>) -> Result<()> { todo!() }
    pub async fn mark_deleted(&self, message_id: i64) -> Result<()> { todo!() }
    pub async fn edit_message(&self, message_id: i64, body: &str) -> Result<()> { todo!() }
    pub async fn set_attachment_path(&self, message_id: i64, index: usize, path: &str) -> Result<()> { todo!() }

    /// Full-text search across message bodies (FTS5), newest first.
    pub async fn search_messages(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> { todo!() }

    pub async fn record_emoji_use(&self, emoji: &str) -> Result<()> { todo!() }
    pub async fn emoji_usage(&self) -> Result<Vec<EmojiUsage>> { todo!() }

    /// Small key/value table for UI settings.
    pub async fn setting(&self, key: &str) -> Result<Option<String>> { todo!() }
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> { todo!() }
}
