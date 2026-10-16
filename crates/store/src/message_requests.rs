//! Message requests, as Signal Desktop keeps them: our answer to a chat's
//! request (`messageRequestResponseType`), whether we reported it, the
//! spam report's token and server GUIDs, and who is in our address book.
//! Whether a chat is accepted is worked out with the chat itself (see
//! [`crate::Conversation::accepted`]).

use crate::{AppDb, ConversationId, Result, ensure_conversation, refresh_preview};

/// Our answer to a message request (Desktop's `messageRequestResponseType`,
/// numbered as `SyncMessage.MessageRequestResponse.Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, serde::Serialize, serde::Deserialize)]
pub enum MessageRequestResponse {
    #[default]
    Unknown,
    Accept,
    Delete,
    Block,
    BlockAndDelete,
    Spam,
    BlockAndSpam,
}

impl MessageRequestResponse {
    /// From the wire (and the database); unknown numbers are `Unknown`.
    pub fn from_wire(value: i64) -> Self {
        match value {
            1 => Self::Accept,
            2 => Self::Delete,
            3 => Self::Block,
            4 => Self::BlockAndDelete,
            5 => Self::Spam,
            6 => Self::BlockAndSpam,
            _ => Self::Unknown,
        }
    }

    pub fn to_wire(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Accept => 1,
            Self::Delete => 2,
            Self::Block => 3,
            Self::BlockAndDelete => 4,
            Self::Spam => 5,
            Self::BlockAndSpam => 6,
        }
    }

    /// It blocks them (Desktop's `hasBlock` in `applyMessageRequestResponse`).
    pub fn blocks(self) -> bool {
        matches!(self, Self::Block | Self::BlockAndDelete | Self::BlockAndSpam)
    }

    /// It reports spam (Desktop's `hasSpam`).
    pub fn spam(self) -> bool {
        matches!(self, Self::Spam | Self::BlockAndSpam)
    }

    /// It deletes the chat's messages.
    pub fn deletes(self) -> bool {
        matches!(self, Self::Delete | Self::BlockAndDelete)
    }
}

/// What a message request answer leaves in the timeline (Desktop's
/// `message-request-response-event` messages, `MessageRequestResponseEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRequestEvent {
    Accept,
    Block,
    Unblock,
    Spam,
}

/// Settings key prefix for someone's report spam token (then their
/// service id).
pub const REPORTING_TOKEN_PREFIX: &str = "reporting_token.";

/// How many server GUIDs a spam report sends: "The server's maximum is 3"
/// (Desktop's `getMessageServerGuidsForSpam`).
pub const SPAM_REPORT_GUIDS: u32 = 3;

impl AppDb {
    /// Records our answer to a chat's message request.
    pub async fn set_message_request(&self, id: &ConversationId, response: MessageRequestResponse) -> Result<()> {
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, id).await?;
        sqlx::query("UPDATE conversations SET message_request = ? WHERE id = ?")
            .bind(response.to_wire())
            .bind(&id.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks a chat reported as spam (Desktop's `isReported`).
    pub async fn set_reported(&self, id: &ConversationId) -> Result<()> {
        sqlx::query("UPDATE conversations SET reported = 1 WHERE id = ?").bind(&id.0).execute(&self.writer).await?;
        Ok(())
    }

    /// Keeps the latest report spam token from someone (Desktop's
    /// `updateReportingToken`), base64, in settings: they may have no chat
    /// with us (a stranger in a group). Returns whether it changed.
    pub async fn set_reporting_token(&self, service_id: &str, token: &str) -> Result<bool> {
        let key = format!("{REPORTING_TOKEN_PREFIX}{service_id}");
        if self.setting(&key).await?.as_deref() == Some(token) {
            return Ok(false);
        }
        self.set_setting(&key, token).await?;
        Ok(true)
    }

    /// Their latest report spam token, if they sent one.
    pub async fn reporting_token(&self, service_id: &str) -> Result<Option<String>> {
        self.setting(&format!("{REPORTING_TOKEN_PREFIX}{service_id}")).await
    }

    /// The server GUIDs a spam report names (Desktop's
    /// `getMessageServerGuidsForSpam`): the newest incoming messages' in the
    /// chat, at most [`SPAM_REPORT_GUIDS`], or, with `author`, only that
    /// person's (in a group, whoever added us).
    pub async fn server_guids_for_spam(&self, conversation: &ConversationId, author: Option<&str>) -> Result<Vec<String>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT server_guid FROM messages \
             WHERE conversation_id = ?1 AND direction = 0 AND server_guid IS NOT NULL \
               AND (?2 IS NULL OR author = ?2) \
             ORDER BY received_at DESC, sent_at DESC LIMIT 20",
        )
        .bind(&conversation.0)
        .bind(author)
        .fetch_all(&self.pool)
        .await?;
        // Desktop selects DISTINCT ones; the same GUID twice is a re-delivery.
        let mut guids = Vec::new();
        for guid in rows {
            if !guids.contains(&guid) {
                guids.push(guid);
            }
            if guids.len() == SPAM_REPORT_GUIDS as usize {
                break;
            }
        }
        Ok(guids)
    }

    /// Who's in our address book now: replaces the list.
    pub async fn replace_system_contacts(&self, service_ids: &[String]) -> Result<bool> {
        let mut tx = self.write().await?;
        let before: Vec<String> = sqlx::query_scalar("SELECT service_id FROM system_contacts ORDER BY service_id")
            .fetch_all(&mut *tx)
            .await?;
        let mut after: Vec<String> = service_ids.to_vec();
        after.sort();
        after.dedup();
        if before == after {
            return Ok(false);
        }
        sqlx::query("DELETE FROM system_contacts").execute(&mut *tx).await?;
        for sid in &after {
            sqlx::query("INSERT INTO system_contacts (service_id) VALUES (?)").bind(sid).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Adds people to our address book list (from transferred history).
    pub async fn add_system_contacts(&self, service_ids: &[String]) -> Result<()> {
        for batch in service_ids.chunks(100) {
            let mut tx = self.write().await?;
            for sid in batch {
                sqlx::query("INSERT OR IGNORE INTO system_contacts (service_id) VALUES (?)").bind(sid).execute(&mut *tx).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    /// Everyone in our address book (service ids).
    pub async fn system_contacts(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT service_id FROM system_contacts").fetch_all(&self.pool).await?)
    }

    /// Deletes a chat's messages but keeps the chat, and with it whether
    /// it's blocked (Desktop's `destroyMessages` for a message request a
    /// block came with).
    pub async fn clear_messages(&self, id: &ConversationId) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?").bind(&id.0).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM pinned_messages WHERE conversation_id = ?").bind(&id.0).execute(&mut *tx).await?;
        sqlx::query("UPDATE conversations SET unread_count = 0, marked_unread = 0 WHERE id = ?").bind(&id.0).execute(&mut *tx).await?;
        refresh_preview(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }
}
