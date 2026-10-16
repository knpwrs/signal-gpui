//! Polls (Desktop's `poll` message attribute): the question and options a
//! message asks, and everyone's votes, kept as JSON on the message.
//!
//! The rules follow Desktop's `ts/messageModifiers/Polls.preload.ts`,
//! `ts/polls/util.std.ts` and `ts/types/Polls.dom.ts`.

use std::collections::{BTreeMap, BTreeSet};

use unicode_segmentation::UnicodeSegmentation as _;

use crate::{AppDb, ConversationId, MESSAGE_COLUMNS, Message, Result, message_from_row, refresh_preview};

/// Desktop's limits (`ts/types/Polls.dom.ts`): a question we send is at
/// most 100 characters, one we receive 200; an option 100; 2 to 10
/// options. Characters are graphemes, and neither may pass Desktop's
/// message body limit in bytes (`MAX_MESSAGE_BODY_BYTE_LENGTH`, 2 KiB).
pub const POLL_QUESTION_MAX_SEND: usize = 100;
pub const POLL_QUESTION_MAX_RECEIVE: usize = 200;
pub const POLL_OPTION_MAX: usize = 100;
pub const POLL_OPTIONS_MIN: usize = 2;
pub const POLL_OPTIONS_MAX: usize = 10;
pub const POLL_TEXT_MAX_BYTES: usize = 2 * 1024;
/// A vote names at most 10 options, each 0 to 9 (Desktop's `PollVoteSchema`).
const VOTE_MAX_INDEX: u32 = 9;

/// A poll and its votes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Poll {
    pub question: String,
    pub options: Vec<String>,
    pub allow_multiple: bool,
    #[serde(default)]
    pub votes: Vec<PollVote>,
    /// Its creator ended it: no more votes ("Poll · Final results").
    #[serde(default)]
    pub ended: bool,
    /// Ours: who it reached (the recipients Desktop's
    /// `sendStateByConversationId` has as sent). Only they may vote in it.
    /// `None` when not known: someone else's poll, or ours from before
    /// this was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reached: Option<BTreeSet<String>>,
}

/// One person's vote: every option they chose. A new vote replaces their
/// last one; `vote_count` orders them (Desktop's `voteCount`).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct PollVote {
    /// The voter's service id; `None` for us.
    pub author: Option<String>,
    pub options: Vec<u32>,
    pub vote_count: u32,
    /// When it was sent (unix millis); 0 when unknown (transferred history).
    pub sent_at: i64,
    /// Ours, still sending. It doesn't count until it's sent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// While ours is sending: each recipient (service id) and whether it
    /// has reached them (Desktop's `sendStateByConversationId` on a vote).
    /// Retries go only to the ones it hasn't.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sends: BTreeMap<String, bool>,
}

impl PollVote {
    /// Who this vote, still sending, hasn't reached yet.
    pub fn unsent(&self) -> Vec<String> {
        self.sends.iter().filter(|(_, sent)| !**sent).map(|(who, _)| who.clone()).collect()
    }
}

/// Where a vote came from (Desktop's `PollSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteSource {
    /// We voted here.
    ThisDevice,
    /// We voted on another of our devices.
    Sync,
    /// Someone else voted.
    Other,
}

/// Votes as the poll shows them (Desktop's `getPollForMessage`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PollTally {
    /// Per option, who chose it (`None` for us), in vote order.
    pub voters: Vec<Vec<Option<String>>>,
    /// Choices counted over everyone (one person can make several).
    pub total_votes: usize,
    /// People who voted.
    pub unique_voters: usize,
    /// The options our sent vote chose.
    pub ours: BTreeSet<u32>,
    /// The options our vote that's still sending chooses.
    pub pending: Option<BTreeSet<u32>>,
}

impl PollTally {
    /// What we've chosen as it shows: the vote that's sending, else the
    /// one that's sent.
    pub fn chosen(&self) -> &BTreeSet<u32> {
        self.pending.as_ref().unwrap_or(&self.ours)
    }

    /// Votes for option `index`.
    pub fn count(&self, index: usize) -> usize {
        self.voters.get(index).map_or(0, Vec::len)
    }

    /// The option's share of voters, 0 to 1 (Desktop divides by unique
    /// voters, so a multiple-choice poll's bars can add up past 100%).
    pub fn share(&self, index: usize) -> f32 {
        if self.unique_voters == 0 { 0. } else { self.count(index) as f32 / self.unique_voters as f32 }
    }

    /// The most votes any option has (the "winning" ones, when above 0).
    pub fn most(&self) -> usize {
        self.voters.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// What a poll says in the chat list and notifications (Desktop's
/// `icu:Poll--preview` after its 📊).
pub fn poll_preview(question: &str) -> String {
    let question: String = question.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("📊 Poll: {question}")
}

/// What the chat list and notifications say for a poll's end, after
/// Desktop's 📊 (`icu:PollTerminate--you` and `icu:PollTerminate--other`).
/// `by`: the name of who ended it, `None` for us.
pub fn poll_ended_preview(by: Option<&str>, question: &str) -> String {
    let question: String = question.split_whitespace().collect::<Vec<_>>().join(" ");
    match by {
        Some(name) => format!("📊 {name} ended the poll: {question}"),
        None => format!("📊 You ended the poll: {question}"),
    }
}

fn graphemes(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Whether a received poll is one Desktop shows (`PollCreateSchema`): it
/// drops the whole message otherwise.
pub fn valid_poll(question: &str, options: &[String]) -> bool {
    let text_ok = |text: &str, max: usize| !text.is_empty() && graphemes(text) <= max && text.len() <= POLL_TEXT_MAX_BYTES;
    text_ok(question, POLL_QUESTION_MAX_RECEIVE)
        && (POLL_OPTIONS_MIN..=POLL_OPTIONS_MAX).contains(&options.len())
        && options.iter().all(|o| text_ok(o, POLL_OPTION_MAX))
}

/// Why a poll can't be sent (Desktop's `PollCreateModal` checks), in its
/// words; `None` when it can. Takes the fields as typed (trimmed here).
pub fn poll_send_error(question: &str, options: &[String]) -> Option<&'static str> {
    let question = question.trim();
    if question.is_empty() {
        return Some("Poll question is required");
    }
    if question.len() > POLL_TEXT_MAX_BYTES || graphemes(question) > POLL_QUESTION_MAX_SEND {
        return Some("Question is too long");
    }
    let filled: Vec<&str> = options.iter().map(|o| o.trim()).filter(|o| !o.is_empty()).collect();
    if filled.len() < POLL_OPTIONS_MIN {
        return Some("Poll requires at least 2 options");
    }
    if filled.len() > POLL_OPTIONS_MAX || filled.iter().any(|o| o.len() > POLL_TEXT_MAX_BYTES || graphemes(o) > POLL_OPTION_MAX) {
        return Some("Option is too long");
    }
    None
}

/// A received vote's options, as Desktop takes them (`processPollVote` and
/// `PollVoteSchema`): duplicates dropped, at most 10, each 0 to 9. `None`
/// drops the vote.
pub fn vote_options(indexes: &[u32]) -> Option<Vec<u32>> {
    let mut unique = Vec::new();
    for &index in indexes {
        if !unique.contains(&index) {
            unique.push(index);
        }
    }
    (unique.len() <= POLL_OPTIONS_MAX && unique.iter().all(|&i| i <= VOTE_MAX_INDEX)).then_some(unique)
}

impl Poll {
    pub fn new(question: String, options: Vec<String>, allow_multiple: bool) -> Self {
        Self { question, options, allow_multiple, votes: Vec::new(), ended: false, reached: None }
    }

    /// Our poll reached these people. Returns whether that's news.
    pub fn record_reached(&mut self, who: impl IntoIterator<Item = String>) -> bool {
        let reached = self.reached.get_or_insert_default();
        let before = reached.len();
        reached.extend(who);
        reached.len() != before
    }

    /// Whether `voter` may vote in our poll (Desktop's
    /// `doesVoteModifierMatchMessage`: one it was sent to). `None` when we
    /// don't know who it reached.
    pub fn reached(&self, voter: &str) -> Option<bool> {
        self.reached.as_ref().map(|r| r.contains(voter))
    }

    /// Whether a vote for these options counts (Desktop's `handlePollVote`):
    /// not after the poll ended, only its own options, each once, and one
    /// at most unless it allows several. No options takes a vote back.
    pub fn accepts(&self, options: &[u32]) -> bool {
        let unique: BTreeSet<u32> = options.iter().copied().collect();
        !self.ended
            && options.iter().all(|&i| (i as usize) < self.options.len())
            && unique.len() == options.len()
            && (self.allow_multiple || options.len() <= 1)
    }

    /// Records a vote, as Desktop's `handlePollVote`: ours from here waits
    /// as pending beside the last one sent; anyone else's (or ours from
    /// another device) replaces their last one when its count is higher,
    /// or, from our other device, the same count sent later. Returns
    /// whether anything changed.
    pub fn apply_vote(&mut self, mut vote: PollVote, source: VoteSource) -> bool {
        if !self.accepts(&vote.options) {
            return false;
        }
        if source == VoteSource::ThisDevice {
            vote.pending = true;
            self.votes.retain(|v| !(v.author == vote.author && v.pending));
            self.votes.push(vote);
            return true;
        }
        vote.pending = false;
        match self.votes.iter().position(|v| v.author == vote.author) {
            Some(ix) => {
                let existing = &self.votes[ix];
                let newer = vote.vote_count > existing.vote_count
                    || (source == VoteSource::Sync && vote.vote_count == existing.vote_count && vote.sent_at > existing.sent_at);
                if newer {
                    self.votes[ix] = vote;
                }
                newer
            }
            None => {
                self.votes.push(vote);
                true
            }
        }
    }

    /// The count for our next vote: one past our highest (Desktop's
    /// `enqueuePollVoteForSend`).
    pub fn next_vote_count(&self) -> u32 {
        self.votes.iter().filter(|v| v.author.is_none()).map(|v| v.vote_count).max().map_or(1, |c| c + 1)
    }

    /// Our vote that's still sending, if there is one.
    pub fn our_pending_vote(&self) -> Option<&PollVote> {
        self.votes.iter().rev().find(|v| v.author.is_none() && v.pending)
    }

    /// Who our vote numbered `vote_count` goes to, if it doesn't say yet
    /// (one from before recipients were kept). Returns whether it changed.
    pub fn address_our_vote(&mut self, vote_count: u32, recipients: &[String]) -> bool {
        match self.votes.iter_mut().find(|v| v.author.is_none() && v.pending && v.vote_count == vote_count) {
            Some(vote) if vote.sends.is_empty() && !recipients.is_empty() => {
                vote.sends = recipients.iter().map(|r| (r.clone(), false)).collect();
                true
            }
            _ => false,
        }
    }

    /// A try at sending our vote numbered `vote_count` reached `reached`;
    /// `unreachable` (no longer registered) never will, so it stops waiting
    /// for them. Once it has reached everyone it counts and replaces our
    /// older votes (Desktop's `markOutgoingPollVoteSent`). Returns whether
    /// anything changed.
    pub fn record_vote_sends(&mut self, vote_count: u32, reached: &[String], unreachable: &[String]) -> bool {
        let Some(ix) = self.votes.iter().position(|v| v.author.is_none() && v.pending && v.vote_count == vote_count) else {
            return false;
        };
        let vote = &mut self.votes[ix];
        for who in reached {
            if let Some(sent) = vote.sends.get_mut(who) {
                *sent = true;
            }
        }
        for who in unreachable {
            vote.sends.remove(who);
        }
        if vote.sends.values().all(|sent| *sent) {
            vote.pending = false;
            vote.sends.clear();
            self.votes.retain(|v| !(v.author.is_none() && !v.pending && v.vote_count < vote_count));
        }
        true
    }

    /// We gave up sending our vote numbered `vote_count` (Desktop's
    /// `markOutgoingPollVoteFailed`): if it reached nobody it goes;
    /// otherwise it stays as sent, since some have it. Returns whether
    /// anything changed.
    pub fn give_up_vote(&mut self, vote_count: u32) -> bool {
        let Some(ix) = self.votes.iter().position(|v| v.author.is_none() && v.pending && v.vote_count == vote_count) else {
            return false;
        };
        let vote = &mut self.votes[ix];
        if vote.sends.values().any(|sent| *sent) {
            vote.pending = false;
            vote.sends.clear();
        } else {
            self.votes.remove(ix);
        }
        true
    }

    /// Ends the poll; `false` when it already had.
    pub fn end(&mut self) -> bool {
        !std::mem::replace(&mut self.ended, true)
    }

    pub fn tally(&self) -> PollTally {
        let mut tally = PollTally { voters: vec![Vec::new(); self.options.len()], ..Default::default() };
        // Our vote still sending shows as our choice, but isn't counted.
        tally.pending = self
            .votes
            .iter()
            .rev()
            .find(|v| v.author.is_none() && v.pending)
            .map(|v| v.options.iter().copied().collect());
        // Each person's latest sent vote: highest count, then newest.
        let mut latest: Vec<&PollVote> = Vec::new();
        for vote in self.votes.iter().filter(|v| !v.pending) {
            match latest.iter_mut().find(|v| v.author == vote.author) {
                Some(existing) => {
                    if (vote.vote_count, vote.sent_at) > (existing.vote_count, existing.sent_at) {
                        *existing = vote;
                    }
                }
                None => latest.push(vote),
            }
        }
        let mut voters = BTreeSet::new();
        for vote in latest {
            let mut seen = BTreeSet::new();
            for &option in &vote.options {
                let Some(list) = tally.voters.get_mut(option as usize) else { continue };
                if !seen.insert(option) {
                    continue;
                }
                list.push(vote.author.clone());
                tally.total_votes += 1;
                voters.insert(vote.author.clone());
                if vote.author.is_none() {
                    tally.ours.insert(option);
                }
            }
        }
        tally.unique_voters = voters.len();
        tally
    }
}

impl AppDb {
    /// Makes message `message_id` a poll. A message that's already one
    /// (delivered again) or deleted keeps what it has. Its chat's preview
    /// follows ("📊 Poll: …").
    pub async fn set_poll(&self, message_id: i64, poll: &Poll) -> Result<()> {
        let mut tx = self.write().await?;
        let conversation: Option<String> = sqlx::query_scalar(
            "UPDATE messages SET poll = ? WHERE id = ? AND poll IS NULL AND deleted = 0 RETURNING conversation_id",
        )
        .bind(serde_json::to_string(poll)?)
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(conversation) = conversation {
            refresh_preview(&mut tx, &ConversationId(conversation)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Changes a poll in one transaction; `f` says whether it changed it.
    /// Returns that, or `false` when the message isn't a poll (any more).
    pub async fn update_poll(&self, message_id: i64, f: impl FnOnce(&mut Poll) -> bool) -> Result<bool> {
        self.change_poll(message_id, false, f).await
    }

    /// As [`AppDb::update_poll`], and when it changes, its votes are unread
    /// until the chat is read (someone else voted in our poll; Desktop's
    /// `hasUnreadPollVotes`).
    pub async fn update_poll_unread(&self, message_id: i64, f: impl FnOnce(&mut Poll) -> bool) -> Result<bool> {
        self.change_poll(message_id, true, f).await
    }

    /// Polls with a vote of ours still sending, to send it again after a
    /// restart.
    pub async fn polls_with_pending_votes(&self) -> Result<Vec<Message>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages INDEXED BY messages_polls \
             WHERE poll IS NOT NULL AND deleted = 0 \
               AND EXISTS (SELECT 1 FROM json_each(poll, '$.votes') v WHERE json_extract(v.value, '$.pending'))"
        )))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_from_row).collect()
    }

    async fn change_poll(&self, message_id: i64, unread: bool, f: impl FnOnce(&mut Poll) -> bool) -> Result<bool> {
        let mut tx = self.write().await?;
        let Some(json) = sqlx::query_scalar::<_, Option<String>>("SELECT poll FROM messages WHERE id = ? AND deleted = 0")
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten()
        else {
            return Ok(false);
        };
        let mut poll: Poll = serde_json::from_str(&json)?;
        if !f(&mut poll) {
            return Ok(false);
        }
        sqlx::query("UPDATE messages SET poll = ?, unread_poll_votes = unread_poll_votes OR ? WHERE id = ?")
            .bind(serde_json::to_string(&poll)?)
            .bind(unread)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The poll `author` (`None`: us) sent at `sent_at`, in any chat, as
    /// Desktop's `findPollMessage` looks one up: by its own timestamp (a
    /// poll can't be edited).
    pub async fn find_poll(&self, author: Option<&str>, sent_at: i64) -> Result<Option<Message>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages \
             WHERE author_key = ? AND sent_at = ? AND poll IS NOT NULL AND deleted = 0 LIMIT 1"
        )))
        .bind(author.unwrap_or(""))
        .bind(sent_at)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(message_from_row).transpose()
    }
}
