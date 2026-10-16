//! Events that came before the message they're for: deletes, edits,
//! reactions, poll votes, receipts and syncs. Desktop keeps each in memory
//! until its message comes (`ts/util/modifyTargetMessage.preload.ts`), and
//! keeps what it came in on disk until then, so a restart doesn't lose it:
//!
//! - Most keep their envelope in the `unprocessed` table, which Desktop
//!   reads again at every start and drops once it's 45 days old or has
//!   been read 10 times (`getAllUnprocessedIds` in `ts/sql/Server.node.ts`).
//! - Receipts, read and view syncs and "delete for me" syncs are sync
//!   tasks, dropped once they've been read 5 times and are 2 days old
//!   (`dequeueOldestSyncTasks`, `MAX_SYNC_TASK_ATTEMPTS`).
//!
//! The store keeps them opaque: the backend's JSON, filed under the author
//! and timestamp of the message it's waiting for.

use crate::{AppDb, Result};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Desktop's `MAX_UNPROCESSED_ATTEMPTS` and its 45-day cutoff.
const ENVELOPE_MAX_ATTEMPTS: i64 = 10;
const ENVELOPE_MAX_AGE_MS: i64 = 45 * DAY_MS;
/// Desktop's `MAX_SYNC_TASK_ATTEMPTS`, and the 2 days a sync task is kept
/// however often it was tried.
const SYNC_MAX_ATTEMPTS: i64 = 5;
const SYNC_MIN_AGE_MS: i64 = 2 * DAY_MS;

/// How long an early event is kept, as Desktop keeps what it came in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyKeep {
    /// Like an envelope in Desktop's `unprocessed` table.
    Envelope,
    /// Like a row in Desktop's `syncTasks` table.
    SyncTask,
}

/// A kept event: its row, and the backend's JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarlyEvent {
    pub id: i64,
    pub action: String,
}

impl AppDb {
    /// Keeps `action` until the message by `author` (`None`: us) sent at
    /// `sent_at` comes. The same event kept twice is kept once.
    pub async fn keep_early_event(
        &self,
        author: Option<&str>,
        sent_at: i64,
        keep: EarlyKeep,
        action: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO early_events (target_author, target_sent_at, sync, action, received_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(author.unwrap_or(""))
        .bind(sent_at)
        .bind(keep == EarlyKeep::SyncTask)
        .bind(action)
        .bind(now)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// The events waiting for the message by `author` sent at `sent_at`,
    /// oldest first.
    pub async fn early_events_for(&self, author: Option<&str>, sent_at: i64) -> Result<Vec<EarlyEvent>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, action FROM early_events WHERE target_sent_at = ? AND target_author = ? ORDER BY id",
        )
        .bind(sent_at)
        .bind(author.unwrap_or(""))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id, action)| EarlyEvent { id, action }).collect())
    }

    /// Every kept event, oldest first.
    pub async fn early_events(&self) -> Result<Vec<EarlyEvent>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, action FROM early_events ORDER BY id").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id, action)| EarlyEvent { id, action }).collect())
    }

    /// At startup, as Desktop reads its caches again: drops what's been
    /// kept long enough, counts an attempt for the rest, and returns them.
    pub async fn load_early_events(&self, now: i64) -> Result<Vec<EarlyEvent>> {
        let mut tx = self.write().await?;
        let dropped = sqlx::query(
            "DELETE FROM early_events WHERE \
             (sync = 0 AND (received_at < ?1 OR attempts >= ?2)) \
             OR (sync = 1 AND attempts >= ?3 AND received_at < ?4)",
        )
        .bind(now - ENVELOPE_MAX_AGE_MS)
        .bind(ENVELOPE_MAX_ATTEMPTS)
        .bind(SYNC_MAX_ATTEMPTS)
        .bind(now - SYNC_MIN_AGE_MS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if dropped > 0 {
            tracing::info!(dropped, "dropped early events whose message never came");
        }
        sqlx::query("UPDATE early_events SET attempts = attempts + 1").execute(&mut *tx).await?;
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, action FROM early_events ORDER BY id").fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|(id, action)| EarlyEvent { id, action }).collect())
    }

    /// An event was applied (or turned out not to apply).
    pub async fn remove_early_event(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM early_events WHERE id = ?").bind(id).execute(&self.writer).await?;
        Ok(())
    }
}
