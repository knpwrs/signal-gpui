//! Unread reminders' queries, as Desktop's
//! `ts/sql/server/unreadReminders.std.ts`: when a chat's unread messages
//! came, and who wrote them, mentioned us or replied to us.

use sqlx::Row as _;

use crate::{AppDb, ConversationId, Result};

/// Desktop's `MAX_SENDERS_TO_SHOW`.
const MAX_SENDERS: i64 = 5;

/// When a chat's unread messages since its last reminder came (unix millis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadRange {
    pub conversation: ConversationId,
    /// The oldest unread message since the last reminder.
    pub oldest_unreminded: i64,
    /// The newest of them.
    pub newest_unread: i64,
}

/// Desktop's `UnreadReminderSummaryData`; people as service ids, most
/// recent first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReminderData {
    pub unread: u32,
    pub mentions: u32,
    pub replies: u32,
    pub senders: Vec<String>,
    pub mentioners: Vec<String>,
    pub repliers: Vec<String>,
}

/// Which unread messages a count or a list of people is about.
#[derive(Clone, Copy)]
enum Filter {
    All,
    Mentions,
    Replies,
}

impl Filter {
    /// Its condition; `?1` is our service id. A quote of ours has no
    /// author.
    fn sql(self) -> &'static str {
        match self {
            Filter::All => "1",
            Filter::Mentions => {
                "EXISTS (SELECT 1 FROM json_each(COALESCE(mentions, '[]')) WHERE json_extract(json_each.value, '$.aci') = ?1)"
            }
            Filter::Replies => "(quote IS NOT NULL AND json_extract(quote, '$.author') IS NULL)",
        }
    }
}

/// Unread messages from others in chat `?2`.
const UNREAD: &str = "conversation_id = ?2 AND is_read = 0 AND direction = 0 AND notice IS NULL";

impl AppDb {
    /// Desktop's `getUnremindedUnreadMessageTimeRanges`: for each chat and
    /// when it was last reminded (or reminders started), when its unread
    /// messages since then came. Chats with none are left out.
    pub async fn unreminded_unread_ranges(&self, chats: &[(ConversationId, i64)]) -> Result<Vec<UnreadRange>> {
        let mut out = Vec::new();
        for (conversation, since) in chats {
            let row = sqlx::query(
                "SELECT MIN(received_at), MAX(received_at) FROM messages \
                 WHERE conversation_id = ? AND is_read = 0 AND direction = 0 AND notice IS NULL AND received_at > ?",
            )
            .bind(&conversation.0)
            .bind(since)
            .fetch_one(&self.pool)
            .await?;
            if let (Some(oldest), Some(newest)) = (row.try_get::<Option<i64>, _>(0)?, row.try_get::<Option<i64>, _>(1)?) {
                out.push(UnreadRange { conversation: conversation.clone(), oldest_unreminded: oldest, newest_unread: newest });
            }
        }
        Ok(out)
    }

    /// Desktop's `getUnreadReminderSummaryData`: a chat's unread messages,
    /// those mentioning `me` and those replying to us, and who wrote them.
    pub async fn unread_reminder_data(&self, conversation: &ConversationId, me: &str) -> Result<ReminderData> {
        let count = |filter: Filter| async move {
            let sql = format!("SELECT count(1) FROM messages WHERE {UNREAD} AND {}", filter.sql());
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql)).bind(me).bind(&conversation.0).fetch_one(&self.pool).await
        };
        let people = |filter: Filter| async move {
            let sql = format!(
                "SELECT author FROM messages WHERE author IS NOT NULL AND {UNREAD} AND {} \
                 GROUP BY author ORDER BY MAX(received_at) DESC LIMIT ?3",
                filter.sql()
            );
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql))
                .bind(me)
                .bind(&conversation.0)
                .bind(MAX_SENDERS)
                .fetch_all(&self.pool)
                .await
        };
        let unread = count(Filter::All).await? as u32;
        let mentions = count(Filter::Mentions).await? as u32;
        let replies = count(Filter::Replies).await? as u32;
        Ok(ReminderData {
            unread,
            mentions,
            replies,
            senders: people(Filter::All).await?,
            mentioners: if mentions > 0 { people(Filter::Mentions).await? } else { Vec::new() },
            repliers: if replies > 0 { people(Filter::Replies).await? } else { Vec::new() },
        })
    }
}
