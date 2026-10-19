//! How far each of our messages got with each recipient, as Signal
//! Desktop keeps `sendStateByConversationId` (`ts/messages/MessageSendState.std.ts`):
//! filled from each send's results, moved on by delivery, read and viewed
//! receipts from that recipient, and read into the bubble's status
//! (`getMessagePropStatus` in `ts/state/selectors/message.preload.ts`).
//! Messages without any (ours from before these were kept, Note to Self)
//! keep the one status they had.

use sqlx::Row as _;

use crate::{AppDb, ConversationId, DeliveryStatus, Result, sent_at_matches, status_to_i64};

/// Desktop's `SendStatus`, in its order (`STATUS_NUMBERS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SendStatus {
    Failed,
    Pending,
    Sent,
    Delivered,
    Read,
    Viewed,
    Skipped,
}

impl SendStatus {
    pub(crate) fn to_i64(self) -> i64 {
        self as i64
    }

    pub(crate) fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Failed,
            1 => Self::Pending,
            2 => Self::Sent,
            3 => Self::Delivered,
            4 => Self::Read,
            5 => Self::Viewed,
            _ => Self::Skipped,
        }
    }

    /// Desktop's `isSent`.
    pub fn is_sent(self) -> bool {
        self >= Self::Sent
    }

    /// Desktop's `sendStateReducer`: a pending send can fail; otherwise the
    /// status only moves forward.
    pub fn advance(self, to: SendStatus) -> SendStatus {
        if self == Self::Pending && to == Self::Failed { to } else { self.max(to) }
    }
}

/// What a send to someone failed of, when it did: their safety number
/// changed (Desktop's `OutgoingIdentityKeyError`, which message details
/// offer to show), or anything else.
pub const SEND_ERROR_IDENTITY: &str = "identity";

/// One recipient's send state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendState {
    /// Their service id.
    pub recipient: String,
    pub status: SendStatus,
    /// When it last changed (unix millis), when known.
    pub updated_at: Option<i64>,
    /// Why it failed: [`SEND_ERROR_IDENTITY`], or a description.
    pub error: Option<String>,
}

/// The bubble's status from its recipients' (Desktop's
/// `getMessagePropStatus`): failed, or partly sent when some failed;
/// otherwise the furthest any got (`getHighestSuccessfulRecipientStatus`,
/// where skipped counts past read). `None` without recipients.
pub fn summarize(states: &[SendState]) -> Option<DeliveryStatus> {
    let highest = states.iter().map(|s| s.status).fold(None::<SendStatus>, |max, s| Some(max.map_or(s, |m| m.max(s))))?;
    // Desktop starts from Pending.
    let highest = highest.max(SendStatus::Pending);
    if states.iter().any(|s| s.status == SendStatus::Failed) {
        return Some(if highest.is_sent() { DeliveryStatus::PartialSent } else { DeliveryStatus::Failed });
    }
    Some(match highest {
        SendStatus::Viewed => DeliveryStatus::Viewed,
        SendStatus::Read | SendStatus::Skipped => DeliveryStatus::Read,
        SendStatus::Delivered => DeliveryStatus::Delivered,
        SendStatus::Sent => DeliveryStatus::Sent,
        SendStatus::Pending | SendStatus::Failed => DeliveryStatus::Sending,
    })
}

impl AppDb {
    /// Each recipient's send state for message `id`.
    pub async fn send_states(&self, id: i64) -> Result<Vec<SendState>> {
        let rows = sqlx::query("SELECT recipient, status, updated_at, error FROM message_send_states WHERE message_id = ? ORDER BY recipient")
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(SendState {
                    recipient: row.try_get("recipient")?,
                    status: SendStatus::from_i64(row.try_get("status")?),
                    updated_at: row.try_get("updated_at")?,
                    error: row.try_get("error")?,
                })
            })
            .collect()
    }

    /// Records how a send of message `id` went for each recipient (at
    /// `at`), moving each on as Desktop's reducer does, and sets the
    /// bubble's status from them all. `error` is kept with a failure and
    /// cleared once they have it. Returns the bubble's status.
    pub async fn record_send_states(
        &self,
        id: i64,
        results: &[(String, SendStatus, Option<String>)],
        at: i64,
    ) -> Result<Option<DeliveryStatus>> {
        let mut tx = self.write().await?;
        for (recipient, status, error) in results {
            let old: Option<i64> = sqlx::query_scalar("SELECT status FROM message_send_states WHERE message_id = ? AND recipient = ?")
                .bind(id)
                .bind(recipient)
                .fetch_optional(&mut *tx)
                .await?;
            let new = match old {
                Some(old) => SendStatus::from_i64(old).advance(*status),
                None => *status,
            };
            if old == Some(new.to_i64()) && new != SendStatus::Failed {
                continue;
            }
            let error = if new == SendStatus::Failed { error.clone() } else { None };
            sqlx::query(
                "INSERT INTO message_send_states (message_id, recipient, status, updated_at, error) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (message_id, recipient) DO UPDATE SET status = ?3, updated_at = ?4, error = ?5",
            )
            .bind(id)
            .bind(recipient)
            .bind(new.to_i64())
            .bind(at)
            .bind(error)
            .execute(&mut *tx)
            .await?;
        }
        let summary = summarize_in(&mut tx, id).await?;
        if let Some(status) = summary {
            sqlx::query("UPDATE messages SET status = ? WHERE id = ?").bind(status_to_i64(status)).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(summary)
    }

    /// Sets message `id`'s send states as transferred history has them
    /// (each with its own time), and the bubble's status from them.
    pub async fn import_send_states(&self, id: i64, states: &[SendState]) -> Result<Option<DeliveryStatus>> {
        let mut tx = self.write().await?;
        for state in states {
            sqlx::query(
                "INSERT OR REPLACE INTO message_send_states (message_id, recipient, status, updated_at, error) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(&state.recipient)
            .bind(state.status.to_i64())
            .bind(state.updated_at)
            .bind(&state.error)
            .execute(&mut *tx)
            .await?;
        }
        let summary = summarize_in(&mut tx, id).await?;
        if let Some(status) = summary {
            sqlx::query("UPDATE messages SET status = ? WHERE id = ?").bind(status_to_i64(status)).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(summary)
    }

    /// A delivery, read or viewed receipt (Desktop's `MessageReceipts`):
    /// `sender` got our messages sent at `sent_at` (an edit's time names
    /// its message too). Their state moves on, and the bubble's status
    /// with it; a message we have no states for (older ones) takes the
    /// receipt's status as before, forward only. Returns the messages
    /// that changed.
    pub async fn apply_receipt_from(&self, sender: &str, sent_at: i64, status: SendStatus, at: i64) -> Result<Vec<(ConversationId, i64)>> {
        let mut tx = self.write().await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            // A deleted message's states are its delete's, which receipts
            // for the message don't move (Desktop keeps
            // `deletedForEveryoneSendStatus` apart).
            "SELECT id, conversation_id, status FROM messages WHERE direction = 1 AND deleted = 0 AND {}",
            sent_at_matches("''", "?1")
        )))
        .bind(sent_at)
        .fetch_all(&mut *tx)
        .await?;
        let mut changed = Vec::new();
        for row in rows {
            let id: i64 = row.try_get("id")?;
            let conversation = ConversationId(row.try_get("conversation_id")?);
            let any: Option<i64> = sqlx::query_scalar("SELECT 1 FROM message_send_states WHERE message_id = ? LIMIT 1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            if any.is_none() {
                // The legacy forward-only rule (`update_status_by_sent_at`).
                let legacy = match status {
                    SendStatus::Delivered => DeliveryStatus::Delivered,
                    SendStatus::Read | SendStatus::Viewed => DeliveryStatus::Read,
                    _ => continue,
                };
                let done = sqlx::query("UPDATE messages SET status = ?1 WHERE id = ?2 AND status != 4 AND status < ?1")
                    .bind(status_to_i64(legacy))
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                if done.rows_affected() > 0 {
                    changed.push((conversation, id));
                }
                continue;
            }
            let old: Option<i64> = sqlx::query_scalar("SELECT status FROM message_send_states WHERE message_id = ? AND recipient = ?")
                .bind(id)
                .bind(sender)
                .fetch_optional(&mut *tx)
                .await?;
            // Desktop ignores a receipt from someone it didn't go to.
            let Some(old) = old.map(SendStatus::from_i64) else { continue };
            let new = old.advance(status);
            if new == old {
                continue;
            }
            sqlx::query("UPDATE message_send_states SET status = ?, updated_at = ?, error = NULL WHERE message_id = ? AND recipient = ?")
                .bind(new.to_i64())
                .bind(at)
                .bind(id)
                .bind(sender)
                .execute(&mut *tx)
                .await?;
            if let Some(summary) = summarize_in(&mut tx, id).await? {
                sqlx::query("UPDATE messages SET status = ? WHERE id = ?").bind(status_to_i64(summary)).bind(id).execute(&mut *tx).await?;
            }
            changed.push((conversation, id));
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// A delete for everyone of message `id` (already deleted) is going
    /// out: the message's recipients' states become the delete's, none yet
    /// (Desktop's `deletedForEveryoneSendStatus`), and it shows as sending.
    pub async fn start_delete_send(&self, id: i64) -> Result<bool> {
        let mut tx = self.write().await?;
        let done = sqlx::query("UPDATE messages SET status = ? WHERE id = ? AND deleted = 1")
            .bind(status_to_i64(DeliveryStatus::Sending))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() > 0 {
            sqlx::query("DELETE FROM message_send_states WHERE message_id = ?").bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(done.rows_affected() > 0)
    }

    /// Recipients of message `id` whose send failed (who Desktop's
    /// "Resend" sends to again).
    pub async fn failed_recipients(&self, id: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT recipient FROM message_send_states WHERE message_id = ? AND status = 0")
            .bind(id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Sets message `id`'s status as it is (a send paused for a captcha,
    /// or failed once the captcha was skipped).
    pub async fn set_message_status(&self, id: i64, status: DeliveryStatus) -> Result<()> {
        sqlx::query("UPDATE messages SET status = ? WHERE id = ?").bind(status_to_i64(status)).bind(id).execute(&self.writer).await?;
        Ok(())
    }

    /// An incoming voice message was played: whether it wasn't before.
    pub async fn mark_viewed(&self, id: i64) -> Result<bool> {
        let done = sqlx::query("UPDATE messages SET viewed = 1 WHERE id = ? AND viewed = 0").bind(id).execute(&self.writer).await?;
        Ok(done.rows_affected() > 0)
    }

    /// Our other device played their message `author`'s `sent_at`: marks it
    /// viewed here. Returns it when it changed.
    pub async fn mark_viewed_by(&self, author: Option<&str>, sent_at: i64) -> Result<Option<(ConversationId, i64)>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE messages SET viewed = 1 WHERE viewed = 0 AND {} RETURNING id, conversation_id",
            sent_at_matches("?1", "?2")
        )))
        .bind(author.unwrap_or(""))
        .bind(sent_at)
        .fetch_optional(&self.writer)
        .await?;
        row.map(|row| Ok((ConversationId(row.try_get("conversation_id")?), row.try_get("id")?))).transpose()
    }
}

async fn summarize_in(conn: &mut sqlx::SqliteConnection, id: i64) -> Result<Option<DeliveryStatus>> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT status FROM message_send_states WHERE message_id = ?").bind(id).fetch_all(&mut *conn).await?;
    let states: Vec<SendState> = rows
        .into_iter()
        .map(|(status,)| SendState { recipient: String::new(), status: SendStatus::from_i64(status), updated_at: None, error: None })
        .collect();
    Ok(summarize(&states))
}
