//! The call history: each call's notice in its chat is also its record
//! (Desktop keeps a `callsHistory` row and a `call-history` message; we keep
//! the one notice). The rules follow Desktop's `ts/util/callDisposition.preload.ts`
//! (status transitions, seen status, disappearing timers) and the call
//! history queries in `ts/sql/Server.node.ts` (`clearCallHistory`,
//! `getUnreadCallMessagesAndMarkRead`).

use sqlx::Row as _;

use crate::{AppDb, CallOutcome, ConversationId, DeliveryStatus, Notice, Result, ensure_conversation, now_millis, status_to_i64};

/// Every call notice (found through the `messages_author` index: notices
/// are a few of all messages).
const CALLS: &str = "author_key = 'notice' AND json_extract(notice, '$.type') IN ('call', 'group_call')";

/// How a group call went for us (Desktop's `GroupCallStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupCallStatus {
    /// Someone started it; we weren't rung and haven't joined.
    Generic,
    /// We rang the group (Desktop has it, though it never sets it).
    OutgoingRing,
    /// It rang us (or, ours, it's ringing them).
    Ringing,
    /// We joined without being rung.
    Joined,
    /// We joined after it rang us, here or on another device.
    Accepted,
    Missed,
    Declined,
}

/// What happened to a call (Desktop's `LocalCallEvent`, and the remote
/// `Accepted`, which moves a group call's status the same way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallHistoryEvent {
    Started,
    Ringing,
    Accepted,
    Declined,
    Missed,
    /// We hung up: incoming, declined; outgoing, missed.
    Hangup,
    /// They hung up: incoming, missed; outgoing, declined.
    RemoteHangup,
}

/// Desktop's `transitionGroupCallStatus`. `incoming`: someone else
/// started (or rang) it.
pub fn transition_group_call(previous: Option<GroupCallStatus>, event: CallHistoryEvent, incoming: bool) -> GroupCallStatus {
    use CallHistoryEvent as E;
    use GroupCallStatus as S;
    if previous == Some(S::Accepted) {
        return S::Accepted;
    }
    match event {
        E::Accepted => match previous {
            None | Some(S::Generic) => S::Joined,
            Some(S::Ringing | S::Missed | S::Declined) => S::Accepted,
            Some(status @ (S::Joined | S::OutgoingRing | S::Accepted)) => status,
        },
        E::Started => S::Generic,
        E::Ringing => S::Ringing,
        E::Missed => S::Missed,
        E::Declined => S::Declined,
        E::Hangup if incoming => S::Declined,
        E::Hangup => S::Missed,
        E::RemoteHangup if incoming => S::Missed,
        E::RemoteHangup => S::Declined,
    }
}

/// A group call that someone else started and that we haven't looked at
/// (Desktop's `isUnseenGroupCall`).
fn group_call_unseen(status: GroupCallStatus, incoming: bool) -> bool {
    incoming && matches!(status, GroupCallStatus::Ringing | GroupCallStatus::Generic | GroupCallStatus::Missed)
}

impl Notice {
    /// Whether someone else started this group call. Older notices kept
    /// only who started it (`None` for us).
    pub fn group_call_incoming(&self) -> bool {
        matches!(self, Notice::GroupCall { by, incoming, .. } if *incoming || by.is_some())
    }

    /// A call not looked at yet (Desktop's unseen call history).
    pub fn call_unseen(&self) -> bool {
        matches!(self, Notice::Call { unseen: true, .. } | Notice::GroupCall { unseen: true, .. })
    }
}

/// One group call, as an event about it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCallRecord {
    /// RingRTC's call id (from the era, or the ring's id).
    pub call_id: i64,
    pub era: Option<String>,
    /// Who started or rang it, when known (`None`: us, or unknown).
    pub by: Option<String>,
    pub incoming: bool,
    /// When it happened (unix millis): a new notice's place in the chat.
    pub at: i64,
}

/// What an event did to a group call's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCallChange {
    /// The notice's row id.
    pub id: i64,
    pub status: GroupCallStatus,
    /// Its status before (`None`: it's new).
    pub previous: Option<GroupCallStatus>,
    /// Someone else started it (as the record says).
    pub incoming: bool,
    /// Its place in the chat: the call's time in the history.
    pub sent_at: i64,
}

/// The call a call log event names (Desktop's `CallLogEventTarget`): the
/// call, or failing that, the latest one at or before `timestamp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTarget {
    pub conversation: Option<ConversationId>,
    pub call_id: Option<i64>,
    pub timestamp: i64,
}

/// A call in the history, to name in a call log event we send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRef {
    pub conversation: ConversationId,
    pub call_id: Option<i64>,
    pub sent_at: i64,
}

/// A call notice's row, read inside a write.
struct Existing {
    id: i64,
    notice: Notice,
    expires_in: u32,
    expire_start: Option<i64>,
    sent_at: i64,
}

async fn existing(conn: &mut sqlx::SqliteConnection, query: &'static str, conversation: &ConversationId, key: impl ToString) -> Result<Option<Existing>> {
    let row = sqlx::query(query).bind(&conversation.0).bind(key.to_string()).fetch_optional(&mut *conn).await?;
    let Some(row) = row else { return Ok(None) };
    let notice: String = row.try_get("notice")?;
    Ok(Some(Existing {
        id: row.try_get("id")?,
        notice: serde_json::from_str(&notice)?,
        expires_in: row.try_get::<i64, _>("expires_in")? as u32,
        expire_start: row.try_get("expire_start")?,
        sent_at: row.try_get("sent_at")?,
    }))
}

/// The chat's disappearing timer, which a new call notice follows
/// (Desktop's `disappearingCalls`).
async fn timer(conn: &mut sqlx::SqliteConnection, conversation: &ConversationId) -> Result<u32> {
    let seconds: Option<i64> = sqlx::query_scalar("SELECT expire_timer FROM conversations WHERE id = ?")
        .bind(&conversation.0)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(seconds.unwrap_or(0).max(0) as u32)
}

/// Inserts a call's notice; notices dedupe by time, so one in the same
/// millisecond as another steps past it.
async fn insert(
    conn: &mut sqlx::SqliteConnection,
    conversation: &ConversationId,
    at: i64,
    notice: &Notice,
    expires_in: u32,
    expire_start: Option<i64>,
) -> Result<Option<(i64, i64)>> {
    let json = serde_json::to_string(notice)?;
    for offset in 0..10 {
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO messages (conversation_id, sent_at, received_at, direction, author_key, body, status, \
                attachments, is_read, notice, expires_in, expire_start) \
             VALUES (?1, ?2, ?2, 0, 'notice', '', ?3, '[]', 1, ?4, ?5, ?6) \
             ON CONFLICT (conversation_id, author_key, sent_at) DO NOTHING RETURNING id",
        )
        .bind(&conversation.0)
        .bind(at + offset)
        .bind(status_to_i64(DeliveryStatus::Read))
        .bind(&json)
        .bind(i64::from(expires_in))
        .bind(expire_start)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = id {
            return Ok(Some((id, at + offset)));
        }
    }
    Ok(None)
}

/// When a call notice's countdown starts: once it's been seen and the
/// call is over (Desktop's `saveCallHistory`).
fn expire_start(expires_in: u32, unseen: bool, active: bool, started: Option<i64>) -> Option<i64> {
    started.or_else(|| (expires_in > 0 && !unseen && !active).then(now_millis))
}

impl AppDb {
    /// Records a 1:1 call's notice, or updates the one for the same call
    /// (seen here and on another device, say). An incoming missed call is
    /// unseen until the call history is read, and once seen stays seen.
    /// A new notice takes the chat's disappearing timer, counting down from
    /// now unless it's unseen. Returns its row id.
    pub async fn record_call(&self, conversation: &ConversationId, at: i64, notice: &Notice) -> Result<Option<i64>> {
        let Notice::Call { incoming, video, outcome, call_id, .. } = notice.clone() else {
            return self.insert_notice(conversation, at, notice).await;
        };
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, conversation).await?;
        let found = match call_id {
            Some(call_id) => {
                existing(
                    &mut tx,
                    "SELECT id, sent_at, notice, expires_in, expire_start FROM messages WHERE conversation_id = ?1 \
                       AND author_key = 'notice' AND json_extract(notice, '$.type') = 'call' \
                       AND json_extract(notice, '$.call_id') = CAST(?2 AS INTEGER)",
                    conversation,
                    call_id,
                )
                .await?
            }
            None => None,
        };
        // Desktop's `isUnseenDirectCall`, and `maxSeenStatus` with the last.
        let unseen = incoming && outcome == CallOutcome::Missed && found.as_ref().is_none_or(|e| e.notice.call_unseen());
        let notice = Notice::Call { incoming, video, outcome, call_id, unseen };
        let id = match found {
            Some(found) => {
                let start = expire_start(found.expires_in, unseen, false, found.expire_start);
                sqlx::query("UPDATE messages SET notice = ?2, expire_start = ?3 WHERE id = ?1")
                    .bind(found.id)
                    .bind(serde_json::to_string(&notice)?)
                    .bind(start)
                    .execute(&mut *tx)
                    .await?;
                Some(found.id)
            }
            None => {
                let expires_in = timer(&mut tx, conversation).await?;
                let start = expire_start(expires_in, unseen, false, None);
                insert(&mut tx, conversation, at, &notice, expires_in, start).await?.map(|(id, _)| id)
            }
        };
        tx.commit().await?;
        Ok(id)
    }

    /// Removes a 1:1 call's notice (deleted from the call history elsewhere).
    pub async fn remove_call(&self, conversation: &ConversationId, call_id: i64) -> Result<Option<i64>> {
        self.remove_call_of(conversation, "call", call_id).await
    }

    /// Removes a group call's notice (deleted from the call history elsewhere).
    pub async fn remove_group_call(&self, conversation: &ConversationId, call_id: i64) -> Result<Option<i64>> {
        self.remove_call_of(conversation, "group_call", call_id).await
    }

    async fn remove_call_of(&self, conversation: &ConversationId, kind: &str, call_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "DELETE FROM messages WHERE conversation_id = ?1 AND author_key = 'notice' \
               AND json_extract(notice, '$.type') = ?3 AND json_extract(notice, '$.call_id') = ?2 RETURNING id",
        )
        .bind(&conversation.0)
        .bind(call_id)
        .bind(kind)
        .fetch_optional(&self.writer)
        .await?)
    }

    /// A group call's event, applied to its record as Desktop's
    /// `updateLocalCallHistory` does: its status moves on, it's unseen if
    /// someone else started it and we haven't joined (and stays seen once
    /// seen), and a new one takes the chat's disappearing timer, which
    /// starts once it's seen and we're not in the call (`active`).
    ///
    /// `only_new`: record it only if there's no record yet (Desktop's
    /// `updateCallHistoryForGroupCallOnPeek`); `None` when there was.
    pub async fn apply_group_call_event(
        &self,
        conversation: &ConversationId,
        record: &GroupCallRecord,
        event: CallHistoryEvent,
        active: bool,
        only_new: bool,
    ) -> Result<Option<GroupCallChange>> {
        let mut tx = self.write().await?;
        ensure_conversation(&mut tx, conversation).await?;
        let found = existing(
            &mut tx,
            "SELECT id, sent_at, notice, expires_in, expire_start FROM messages WHERE conversation_id = ?1 \
               AND author_key = 'notice' AND json_extract(notice, '$.type') = 'group_call' \
               AND json_extract(notice, '$.call_id') = CAST(?2 AS INTEGER)",
            conversation,
            record.call_id,
        )
        .await?;
        // Older notices know only the era.
        let found = match (found, &record.era) {
            (None, Some(era)) => {
                existing(
                    &mut tx,
                    "SELECT id, sent_at, notice, expires_in, expire_start FROM messages WHERE conversation_id = ?1 \
                       AND author_key = 'notice' AND json_extract(notice, '$.type') = 'group_call' \
                       AND json_extract(notice, '$.era') = ?2",
                    conversation,
                    era,
                )
                .await?
            }
            (found, _) => found,
        };
        if only_new && found.is_some() {
            return Ok(None);
        }
        let (by, era, previous, incoming, was_unseen) = match &found {
            Some(Existing { notice: notice @ Notice::GroupCall { by, era, status, .. }, .. }) => (
                by.clone().or_else(|| record.by.clone()),
                era.clone().or_else(|| record.era.clone()),
                // A notice from before statuses: someone started a call.
                Some(status.unwrap_or(GroupCallStatus::Generic)),
                notice.group_call_incoming(),
                notice.call_unseen(),
            ),
            _ => (record.by.clone(), record.era.clone(), None, record.incoming, true),
        };
        let status = transition_group_call(previous, event, incoming);
        let unseen = group_call_unseen(status, incoming) && was_unseen;
        let notice = Notice::GroupCall { by, era, call_id: Some(record.call_id), status: Some(status), incoming, unseen };
        let id = match found {
            Some(found) => {
                let start = expire_start(found.expires_in, unseen, active, found.expire_start);
                if found.notice != notice || start != found.expire_start {
                    sqlx::query("UPDATE messages SET notice = ?2, expire_start = ?3 WHERE id = ?1")
                        .bind(found.id)
                        .bind(serde_json::to_string(&notice)?)
                        .bind(start)
                        .execute(&mut *tx)
                        .await?;
                }
                Some((found.id, found.sent_at))
            }
            None => {
                let expires_in = timer(&mut tx, conversation).await?;
                let start = expire_start(expires_in, unseen, active, None);
                insert(&mut tx, conversation, record.at, &notice, expires_in, start).await?
            }
        };
        tx.commit().await?;
        Ok(id.map(|(id, sent_at)| GroupCallChange { id, status, previous, incoming, sent_at }))
    }

    /// The latest call in the history (in one chat, or all), to name in a
    /// call log event.
    pub async fn latest_call(&self, conversation: Option<&ConversationId>) -> Result<Option<CallRef>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT conversation_id, sent_at, notice FROM messages WHERE {CALLS} \
               AND (?1 IS NULL OR conversation_id = ?1) ORDER BY sent_at DESC LIMIT 1"
        )))
            .bind(conversation.map(|c| c.0.clone()))
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| call_ref(&row)).transpose()
    }

    /// The latest unseen call in a chat (to mark the chat's calls read).
    pub async fn latest_unseen_call(&self, conversation: &ConversationId) -> Result<Option<CallRef>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT conversation_id, sent_at, notice FROM messages WHERE {CALLS} AND conversation_id = ?1 \
               AND json_extract(notice, '$.unseen') = 1 ORDER BY sent_at DESC LIMIT 1"
        )))
        .bind(&conversation.0)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| call_ref(&row)).transpose()
    }

    /// How many calls are unseen (Desktop's call history unread count, the
    /// Calls tab's badge).
    pub async fn unseen_calls(&self) -> Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM messages WHERE {CALLS} AND json_extract(notice, '$.unseen') = 1"
        )))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Marks calls up to `target` seen, in its chat (`in_conversation`) or
    /// everywhere, and starts the countdown of disappearing ones, except
    /// the call we're in (`active`). Desktop's
    /// `getUnreadCallMessagesAndMarkRead`. Returns the notices changed.
    pub async fn mark_calls_seen(
        &self,
        target: &CallTarget,
        in_conversation: bool,
        read_at: i64,
        active: Option<(&ConversationId, i64)>,
    ) -> Result<Vec<(ConversationId, i64)>> {
        let mut tx = self.write().await?;
        let Some((conversation, up_to)) = find_target(&mut tx, target).await? else { return Ok(Vec::new()) };
        // ?2: the chat, or NULL for every chat.
        let scope = " AND (?2 IS NULL OR conversation_id = ?2)";
        let chat = in_conversation.then(|| conversation.0.clone());
        let seen: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE messages SET notice = json_remove(notice, '$.unseen') \
             WHERE {CALLS} AND sent_at <= ?1{scope} AND json_extract(notice, '$.unseen') = 1 \
             RETURNING id, conversation_id"
        )))
        .bind(up_to)
        .bind(&chat)
        .fetch_all(&mut *tx)
        .await?;
        let (active_conversation, active_call) = active.map_or((String::new(), 0), |(c, id)| (c.0.clone(), id));
        let started: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE messages SET expire_start = ?3 \
             WHERE {CALLS} AND sent_at <= ?1{scope} AND expires_in > 0 AND expire_start IS NULL \
               AND NOT (conversation_id = ?4 AND json_extract(notice, '$.call_id') IS ?5) \
             RETURNING id, conversation_id"
        )))
        .bind(up_to)
        .bind(&chat)
        .bind(read_at)
        .bind(active_conversation)
        .bind(active.map(|_| active_call))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut changed: Vec<(ConversationId, i64)> =
            seen.into_iter().chain(started).map(|(id, c)| (ConversationId(c), id)).collect();
        changed.sort_by_key(|(_, id)| *id);
        changed.dedup();
        Ok(changed)
    }

    /// Deletes every call up to `target` from the history, in every chat
    /// (Desktop's `clearCallHistory`). Returns the notices removed.
    pub async fn clear_call_history(&self, target: &CallTarget) -> Result<Vec<(ConversationId, i64)>> {
        let mut tx = self.write().await?;
        let Some((_, up_to)) = find_target(&mut tx, target).await? else { return Ok(Vec::new()) };
        let removed: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "DELETE FROM messages WHERE {CALLS} AND sent_at <= ?1 RETURNING id, conversation_id"
        )))
        .bind(up_to)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(removed.into_iter().map(|(id, c)| (ConversationId(c), id)).collect())
    }
}

fn call_ref(row: &sqlx::sqlite::SqliteRow) -> Result<CallRef> {
    let notice: Notice = serde_json::from_str(row.try_get("notice")?)?;
    let call_id = match notice {
        Notice::Call { call_id, .. } | Notice::GroupCall { call_id, .. } => call_id,
        _ => None,
    };
    Ok(CallRef { conversation: ConversationId(row.try_get("conversation_id")?), call_id, sent_at: row.try_get("sent_at")? })
}

/// The call a target names: its chat and time. Desktop's
/// `getCallHistoryForCallLogEventTarget`: the call itself when its id is
/// given (nothing if it's not here), else the latest at or before the time.
async fn find_target(conn: &mut sqlx::SqliteConnection, target: &CallTarget) -> Result<Option<(ConversationId, i64)>> {
    let row: Option<(String, i64)> = match (&target.conversation, target.call_id) {
        (Some(conversation), Some(call_id)) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT conversation_id, sent_at FROM messages WHERE {CALLS} AND conversation_id = ?1 \
                   AND json_extract(notice, '$.call_id') = ?2 LIMIT 1"
            )))
            .bind(&conversation.0)
            .bind(call_id)
            .fetch_optional(&mut *conn)
            .await?
        }
        (Some(conversation), None) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT conversation_id, sent_at FROM messages WHERE {CALLS} AND conversation_id = ?1 \
                   AND sent_at <= ?2 ORDER BY sent_at DESC LIMIT 1"
            )))
            .bind(&conversation.0)
            .bind(target.timestamp)
            .fetch_optional(&mut *conn)
            .await?
        }
        (None, _) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT conversation_id, sent_at FROM messages WHERE {CALLS} AND sent_at <= ?1 ORDER BY sent_at DESC LIMIT 1"
            )))
            .bind(target.timestamp)
            .fetch_optional(&mut *conn)
            .await?
        }
    };
    Ok(row.map(|(c, at)| (ConversationId(c), at)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use CallHistoryEvent as E;
    use GroupCallStatus as S;

    #[test]
    fn group_call_statuses_move_as_desktops_do() {
        // Rung, then joined: accepted; and accepted stays.
        assert_eq!(transition_group_call(None, E::Ringing, true), S::Ringing);
        assert_eq!(transition_group_call(Some(S::Ringing), E::Accepted, true), S::Accepted);
        assert_eq!(transition_group_call(Some(S::Accepted), E::Missed, true), S::Accepted);
        assert_eq!(transition_group_call(Some(S::Accepted), E::Started, true), S::Accepted);
        // Joined without a ring.
        assert_eq!(transition_group_call(None, E::Accepted, true), S::Joined);
        assert_eq!(transition_group_call(Some(S::Generic), E::Accepted, true), S::Joined);
        assert_eq!(transition_group_call(Some(S::Joined), E::Accepted, true), S::Joined);
        // Missed and declined rings become accepted when joined.
        assert_eq!(transition_group_call(Some(S::Missed), E::Accepted, true), S::Accepted);
        assert_eq!(transition_group_call(Some(S::Declined), E::Accepted, true), S::Accepted);
        // Leaving a joined call (Desktop's `Started` on `NotJoined`).
        assert_eq!(transition_group_call(Some(S::Joined), E::Started, true), S::Generic);
        assert_eq!(transition_group_call(Some(S::Ringing), E::Missed, true), S::Missed);
        assert_eq!(transition_group_call(None, E::Declined, true), S::Declined);
        assert_eq!(transition_group_call(None, E::Hangup, true), S::Declined);
        assert_eq!(transition_group_call(None, E::Hangup, false), S::Missed);
        assert_eq!(transition_group_call(None, E::RemoteHangup, true), S::Missed);
        assert_eq!(transition_group_call(None, E::RemoteHangup, false), S::Declined);
    }

    #[test]
    fn only_others_calls_we_missed_or_left_alone_are_unseen() {
        assert!(group_call_unseen(S::Ringing, true));
        assert!(group_call_unseen(S::Generic, true));
        assert!(group_call_unseen(S::Missed, true));
        assert!(!group_call_unseen(S::Declined, true));
        assert!(!group_call_unseen(S::Joined, true));
        assert!(!group_call_unseen(S::Ringing, false));
    }

    #[test]
    fn older_group_call_notices_still_read() {
        let old: Notice = serde_json::from_str(r#"{"type":"group_call","by":"alice","era":"e1"}"#).unwrap();
        assert!(old.group_call_incoming());
        assert!(!old.call_unseen());
        let ours: Notice = serde_json::from_str(r#"{"type":"group_call","by":null}"#).unwrap();
        assert!(!ours.group_call_incoming());
    }
}
