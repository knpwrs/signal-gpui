//! View-once messages (Desktop's `isViewOnce`): a photo or video that can
//! be opened once, then is erased (`ts/util/cleanup.preload.ts`'s
//! `eraseMessageContents`). Theirs are erased once viewed, here or on
//! another of our devices; ours once sent; and any left unviewed once they
//! are older than the server keeps messages
//! (`ts/services/tapToViewMessagesDeletionService.preload.ts`).

use crate::{AppDb, AttachmentMeta, ConversationId, Result, refresh_preview};

/// Where a view-once message is in its one viewing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewOnce {
    /// Its media is here (or still to download) to be viewed once.
    Ready,
    /// Viewed, here or on another device (Desktop's `ReadStatus.Viewed`):
    /// its media is gone.
    Viewed,
    /// Gone without being viewed: too old, or ours, erased once sent
    /// (Desktop's `isErased`).
    Gone,
    /// Theirs, but not something that can be viewed once (Desktop's
    /// `isTapToViewInvalid`): nothing was kept.
    Invalid,
}

impl ViewOnce {
    pub(crate) fn to_i64(self) -> i64 {
        match self {
            Self::Ready => 0,
            Self::Viewed => 1,
            Self::Gone => 2,
            Self::Invalid => 3,
        }
    }

    pub(crate) fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Ready,
            1 => Self::Viewed,
            3 => Self::Invalid,
            _ => Self::Gone,
        }
    }
}

/// The least time an unviewed view-once message is kept: Desktop's
/// `getMessageQueueTime` follows the server's message queue time, never
/// under 30 days (the backend's remote config reads the rest).
pub const VIEW_ONCE_MIN_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What a view-once message says in the chat list and notifications, in
/// Desktop's words (`getNotificationDataForMessage`): its kind while it
/// can be viewed, "View-once Media" once erased.
pub fn view_once_preview(view_once: ViewOnce, first: Option<&AttachmentMeta>) -> &'static str {
    let kind = first.filter(|_| view_once == ViewOnce::Ready).map(|a| a.content_type.as_str());
    match kind {
        Some(kind) if kind.starts_with("image/") => "📷 View-once Photo",
        Some(kind) if kind.starts_with("video/") => "🎥 View-once Video",
        Some(_) => "📎 Media Message",
        None => "View-once Media",
    }
}

/// A view-once message erased: its chat, and the files it had, to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasedViewOnce {
    pub conversation: ConversationId,
    pub files: Vec<String>,
}

impl AppDb {
    /// Erases a view-once message that's still [`ViewOnce::Ready`], moving
    /// it to `to`: its media (and any text) goes, its reactions stay.
    /// Viewing ([`ViewOnce::Viewed`]) is only for theirs, and also reads
    /// it, as Desktop's `markViewOnceMessageViewed` does. `Ok(None)` when
    /// there was nothing to erase (already erased, or not view-once).
    pub async fn erase_view_once(&self, message_id: i64, to: ViewOnce) -> Result<Option<ErasedViewOnce>> {
        let viewed = to == ViewOnce::Viewed;
        let mut tx = self.write().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT conversation_id, attachments FROM messages \
             WHERE id = ?1 AND view_once = 0 AND (?2 = 0 OR direction = 0)",
        )
        .bind(message_id)
        .bind(viewed)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((conversation, attachments)) = row else { return Ok(None) };
        let attachments: Vec<AttachmentMeta> = serde_json::from_str(&attachments)?;
        sqlx::query(
            "UPDATE messages SET view_once = ?2, attachments = '[]', body = '', link_preview = NULL, quote = NULL, \
                mentions = NULL, styles = NULL, long_text = NULL, \
                expire_start = CASE WHEN ?3 AND is_read = 0 AND expires_in > 0 \
                    THEN CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) ELSE expire_start END, \
                is_read = CASE WHEN ?3 THEN 1 ELSE is_read END \
             WHERE id = ?1",
        )
        .bind(message_id)
        .bind(to.to_i64())
        .bind(viewed)
        .execute(&mut *tx)
        .await?;
        if viewed {
            sqlx::query(
                "UPDATE conversations SET unread_count = \
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND is_read = 0) \
                 WHERE id = ?1",
            )
            .bind(&conversation)
            .execute(&mut *tx)
            .await?;
        }
        let conversation = ConversationId(conversation);
        refresh_preview(&mut tx, &conversation).await?;
        tx.commit().await?;
        let files = attachments.into_iter().filter_map(|a| a.local_path).collect();
        Ok(Some(ErasedViewOnce { conversation, files }))
    }

    /// View-once messages still to erase that arrived at or before
    /// `received_before` (unix millis), oldest first.
    pub async fn view_once_to_erase(&self, received_before: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM messages WHERE view_once = 0 AND received_at <= ? ORDER BY received_at",
        )
        .bind(received_before)
        .fetch_all(&self.pool)
        .await?)
    }

    /// When the oldest view-once message still to erase arrived (unix
    /// millis), if there is one (Desktop's
    /// `getNextTapToViewMessageTimestampToAgeOut`).
    pub async fn oldest_view_once(&self) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT MIN(received_at) FROM messages WHERE view_once = 0").fetch_one(&self.pool).await?)
    }
}
