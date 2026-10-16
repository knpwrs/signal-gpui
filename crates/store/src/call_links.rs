//! Call links (Desktop's `callLinks` table) and the calls made through them
//! (its call history's adhoc calls). A link's root key is its secret: the
//! `https://signal.link/call/#key=…` URL carries it, and its room id (a hash
//! of it) names the call to the calling server.

use sqlx::Row as _;

use crate::{AppDb, Result};

/// Who may join a link's call (Desktop's `CallLinkRestrictions`; the
/// numbers are RingRTC's and Desktop's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallLinkRestrictions {
    /// Anyone with the link.
    None,
    /// An admin approves each new person.
    AdminApproval,
    /// Not read from the calling server yet.
    #[default]
    Unknown,
}

impl CallLinkRestrictions {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::None => 0,
            Self::AdminApproval => 1,
            Self::Unknown => 2,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::None,
            1 => Self::AdminApproval,
            _ => Self::Unknown,
        }
    }
}

/// A call link we made or used.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallLink {
    /// The room id, in lowercase hex (Desktop's `roomId`).
    pub room_id: String,
    /// The root key's bytes (what Storage Service and sync messages carry).
    pub root_key: Vec<u8>,
    /// Ours to change and delete: the admin passkey.
    pub admin_key: Option<Vec<u8>>,
    /// Empty: unnamed (Desktop shows "Signal call").
    pub name: String,
    pub restrictions: CallLinkRestrictions,
    pub revoked: bool,
    /// Unix millis; `None` until the calling server says.
    pub expiration: Option<i64>,
    /// Unix millis, for a link we deleted.
    pub deleted_at: Option<i64>,
    /// When we learned of it (unix millis).
    pub added_at: i64,
}

impl CallLink {
    pub fn is_admin(&self) -> bool {
        self.admin_key.as_ref().is_some_and(|k| !k.is_empty())
    }
}

/// How a call through a link went (Desktop's `AdhocCallStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdhocCallStatus {
    /// A link we made and haven't called through (Desktop's placeholder).
    Pending,
    /// We were in it.
    Joined,
    /// Someone was in it; we weren't.
    Generic,
}

impl AdhocCallStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Joined => "joined",
            Self::Generic => "generic",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "joined" => Self::Joined,
            _ => Self::Generic,
        }
    }

    /// Joined outranks the others: a call we were in stays joined.
    fn rank(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::Generic => 1,
            Self::Joined => 2,
        }
    }
}

/// A call through a link, for the Calls tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdhocCall {
    pub room_id: String,
    /// RingRTC's id for the call (from its era), its bits as an i64.
    pub call_id: i64,
    /// Unix millis.
    pub at: i64,
    pub status: AdhocCallStatus,
}

const COLUMNS: &str = "room_id, root_key, admin_key, name, restrictions, revoked, expiration, deleted_at, added_at";

fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<CallLink> {
    Ok(CallLink {
        room_id: row.try_get("room_id")?,
        root_key: row.try_get("root_key")?,
        admin_key: row.try_get("admin_key")?,
        name: row.try_get("name")?,
        restrictions: CallLinkRestrictions::from_i64(row.try_get("restrictions")?),
        revoked: row.try_get::<i64, _>("revoked")? != 0,
        expiration: row.try_get("expiration")?,
        deleted_at: row.try_get("deleted_at")?,
        added_at: row.try_get("added_at")?,
    })
}

impl AppDb {
    /// Call links not deleted, newest first.
    pub async fn call_links(&self) -> Result<Vec<CallLink>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM call_links WHERE deleted_at IS NULL ORDER BY added_at DESC"
        )))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(from_row).collect()
    }

    /// A call link by room id, deleted or not.
    pub async fn call_link(&self, room_id: &str) -> Result<Option<CallLink>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {COLUMNS} FROM call_links WHERE room_id = ?")))
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(from_row).transpose()
    }

    /// Adds a link we learned of (made, synced, opened), as Desktop's
    /// `insertOrUpdateCallLinkFromSync`: a known one keeps its state, and
    /// takes the admin key if this one has it and it didn't. A deleted one
    /// stays deleted. Returns whether it's new.
    pub async fn add_call_link(&self, link: &CallLink) -> Result<bool> {
        let mut tx = self.write().await?;
        let existing: Option<(Option<Vec<u8>>, Option<i64>)> =
            sqlx::query_as("SELECT admin_key, deleted_at FROM call_links WHERE room_id = ?")
                .bind(&link.room_id)
                .fetch_optional(&mut *tx)
                .await?;
        let inserted = match existing {
            None => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO call_links ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                )))
                .bind(&link.room_id)
                .bind(&link.root_key)
                .bind(&link.admin_key)
                .bind(&link.name)
                .bind(link.restrictions.to_i64())
                .bind(link.revoked as i64)
                .bind(link.expiration)
                .bind(link.deleted_at)
                .bind(link.added_at)
                .execute(&mut *tx)
                .await?;
                true
            }
            Some((admin_key, deleted_at)) => {
                if deleted_at.is_none() && admin_key.is_none() && link.is_admin() {
                    sqlx::query("UPDATE call_links SET admin_key = ? WHERE room_id = ?")
                        .bind(&link.admin_key)
                        .bind(&link.room_id)
                        .execute(&mut *tx)
                        .await?;
                }
                false
            }
        };
        tx.commit().await?;
        Ok(inserted)
    }

    /// The calling server's word on a link: its name, approval setting,
    /// whether it's revoked, and when it expires. `false`: no such link, or
    /// nothing changed.
    pub async fn set_call_link_state(
        &self,
        room_id: &str,
        name: &str,
        restrictions: CallLinkRestrictions,
        revoked: bool,
        expiration: Option<i64>,
    ) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE call_links SET name = ?2, restrictions = ?3, revoked = ?4, expiration = ?5 WHERE room_id = ?1 \
               AND (name IS NOT ?2 OR restrictions IS NOT ?3 OR revoked IS NOT ?4 OR expiration IS NOT ?5)",
        )
        .bind(room_id)
        .bind(name)
        .bind(restrictions.to_i64())
        .bind(revoked as i64)
        .bind(expiration)
        .execute(&self.writer)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Marks a link deleted (here, or on another device) and drops its
    /// calls from the history, as Desktop's `markCallLinkDeleted` and its
    /// cleanup do. The admin key goes: Desktop writes none for a deleted link.
    pub async fn mark_call_link_deleted(&self, room_id: &str, at: i64) -> Result<bool> {
        let mut tx = self.write().await?;
        let done = sqlx::query(
            "UPDATE call_links SET deleted_at = ?2, admin_key = NULL WHERE room_id = ?1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM adhoc_calls WHERE room_id = ?").bind(room_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(done.rows_affected() > 0)
    }

    /// Forgets a link we only joined (not ours), with its calls.
    pub async fn remove_call_link(&self, room_id: &str) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM adhoc_calls WHERE room_id = ?").bind(room_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM call_links WHERE room_id = ?").bind(room_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records a call through a link, or updates the one for the same call.
    /// A status only moves up (pending, generic, joined); the time stays the
    /// first one seen. A real call replaces the link's pending placeholder.
    /// `false`: nothing changed.
    pub async fn record_adhoc_call(&self, call: &AdhocCall) -> Result<bool> {
        let mut tx = self.write().await?;
        let existing: Option<String> = sqlx::query_scalar("SELECT status FROM adhoc_calls WHERE room_id = ? AND call_id = ?")
            .bind(&call.room_id)
            .bind(call.call_id)
            .fetch_optional(&mut *tx)
            .await?;
        let changed = match existing.map(|s| AdhocCallStatus::parse(&s)) {
            Some(old) if old.rank() >= call.status.rank() => false,
            Some(_) => {
                sqlx::query("UPDATE adhoc_calls SET status = ? WHERE room_id = ? AND call_id = ?")
                    .bind(call.status.as_str())
                    .bind(&call.room_id)
                    .bind(call.call_id)
                    .execute(&mut *tx)
                    .await?;
                true
            }
            None => {
                sqlx::query("INSERT INTO adhoc_calls (room_id, call_id, at, status) VALUES (?, ?, ?, ?)")
                    .bind(&call.room_id)
                    .bind(call.call_id)
                    .bind(call.at)
                    .bind(call.status.as_str())
                    .execute(&mut *tx)
                    .await?;
                true
            }
        };
        if changed && call.status != AdhocCallStatus::Pending {
            sqlx::query("DELETE FROM adhoc_calls WHERE room_id = ? AND status = 'pending'")
                .bind(&call.room_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Calls through links still kept, newest first.
    pub async fn adhoc_call_history(&self, limit: u32) -> Result<Vec<AdhocCall>> {
        let rows = sqlx::query(
            "SELECT a.room_id, a.call_id, a.at, a.status FROM adhoc_calls a \
               JOIN call_links l ON l.room_id = a.room_id AND l.deleted_at IS NULL \
             ORDER BY a.at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(AdhocCall {
                    room_id: row.try_get("room_id")?,
                    call_id: row.try_get("call_id")?,
                    at: row.try_get("at")?,
                    status: AdhocCallStatus::parse(&row.try_get::<String, _>("status")?),
                })
            })
            .collect()
    }

    /// Desktop's `clearCallHistory` for links: calls up to `up_to` (unix
    /// millis) and unused links' placeholders go, except those of links
    /// we're admin of (which are deleted on the server instead). `true`:
    /// some went.
    pub async fn clear_adhoc_calls(&self, up_to: i64) -> Result<bool> {
        let done = sqlx::query(
            "DELETE FROM adhoc_calls WHERE (at <= ?1 OR status = 'pending') \
               AND room_id NOT IN (SELECT room_id FROM call_links WHERE admin_key IS NOT NULL)",
        )
        .bind(up_to)
        .execute(&self.writer)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Drops a link's calls from the history (Desktop's "Delete" on a row,
    /// or a call event sync's clear), keeping the link.
    pub async fn remove_adhoc_calls(&self, room_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM adhoc_calls WHERE room_id = ?").bind(room_id).execute(&self.writer).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> AppDb {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = AppDb::open(&dir.join("app.db"), &[3; 32]).await.unwrap();
        std::mem::forget(db.clone());
        db
    }

    fn link(room: &str, admin: bool) -> CallLink {
        CallLink {
            room_id: room.into(),
            root_key: vec![1; 16],
            admin_key: admin.then(|| vec![2; 16]),
            added_at: 10,
            ..Default::default()
        }
    }

    fn call_of(room: &str, call_id: i64, at: i64) -> AdhocCall {
        AdhocCall { room_id: room.into(), call_id, at, status: AdhocCallStatus::Joined }
    }

    #[tokio::test]
    async fn links_are_added_once_and_take_their_state_from_the_server() {
        let db = test_db().await;
        assert!(db.add_call_link(&link("aa", false)).await.unwrap());
        // Hearing of it again (a sync from another device) adds the admin key.
        assert!(!db.add_call_link(&link("aa", true)).await.unwrap());
        let stored = db.call_link("aa").await.unwrap().unwrap();
        assert!(stored.is_admin());
        assert_eq!(stored.restrictions, CallLinkRestrictions::Unknown);

        assert!(db.set_call_link_state("aa", "Book club", CallLinkRestrictions::AdminApproval, false, Some(99)).await.unwrap());
        let stored = db.call_link("aa").await.unwrap().unwrap();
        assert_eq!((stored.name.as_str(), stored.restrictions, stored.expiration), ("Book club", CallLinkRestrictions::AdminApproval, Some(99)));
        assert!(!db.set_call_link_state("aa", "Book club", CallLinkRestrictions::AdminApproval, false, Some(99)).await.unwrap(), "unchanged");
        assert!(!db.set_call_link_state("bb", "", CallLinkRestrictions::None, false, None).await.unwrap());
    }

    #[tokio::test]
    async fn deleted_links_leave_the_list_and_the_history() {
        let db = test_db().await;
        db.add_call_link(&link("aa", true)).await.unwrap();
        db.record_adhoc_call(&AdhocCall { room_id: "aa".into(), call_id: 1, at: 5, status: AdhocCallStatus::Joined })
            .await
            .unwrap();
        assert!(db.mark_call_link_deleted("aa", 20).await.unwrap());
        assert!(!db.mark_call_link_deleted("aa", 30).await.unwrap(), "deleted once");
        assert!(db.call_links().await.unwrap().is_empty());
        assert!(db.adhoc_call_history(10).await.unwrap().is_empty());
        let stored = db.call_link("aa").await.unwrap().unwrap();
        assert_eq!((stored.deleted_at, stored.admin_key), (Some(20), None));
        // A sync for it doesn't bring it back.
        assert!(!db.add_call_link(&link("aa", true)).await.unwrap());
        assert!(db.call_links().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn adhoc_calls_only_move_up_and_replace_the_placeholder() {
        let db = test_db().await;
        db.add_call_link(&link("aa", true)).await.unwrap();
        let call = |call_id, at, status| AdhocCall { room_id: "aa".into(), call_id, at, status };
        assert!(db.record_adhoc_call(&call(7, 1, AdhocCallStatus::Pending)).await.unwrap());
        assert!(db.record_adhoc_call(&call(8, 5, AdhocCallStatus::Generic)).await.unwrap());
        assert!(db.record_adhoc_call(&call(8, 6, AdhocCallStatus::Joined)).await.unwrap());
        assert!(!db.record_adhoc_call(&call(8, 7, AdhocCallStatus::Generic)).await.unwrap());
        let history = db.adhoc_call_history(10).await.unwrap();
        assert_eq!(history, [call(8, 5, AdhocCallStatus::Joined)]);

        // Clearing the history keeps our own links' calls.
        assert!(!db.clear_adhoc_calls(100).await.unwrap());
        db.add_call_link(&link("bb", false)).await.unwrap();
        db.record_adhoc_call(&call_of("bb", 1, 50)).await.unwrap();
        db.record_adhoc_call(&call_of("bb", 2, 150)).await.unwrap();
        assert!(db.clear_adhoc_calls(100).await.unwrap());
        let left: Vec<(String, i64)> = db.adhoc_call_history(10).await.unwrap().into_iter().map(|c| (c.room_id, c.call_id)).collect();
        assert_eq!(left, [("bb".to_owned(), 2), ("aa".to_owned(), 8)]);

        db.remove_adhoc_calls("aa").await.unwrap();
        db.remove_adhoc_calls("bb").await.unwrap();
        assert!(db.adhoc_call_history(10).await.unwrap().is_empty());
        assert_eq!(db.call_links().await.unwrap().len(), 2, "the links stay");
        db.remove_call_link("aa").await.unwrap();
        assert!(db.call_link("aa").await.unwrap().is_none());
    }
}
