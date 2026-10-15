//! Polls, as Desktop applies them (`ts/messageModifiers/Polls.preload.ts`)
//! and sends them (`ts/util/enqueuePollCreateForSend.dom.ts`,
//! `ts/polls/enqueuePollVoteForSend.preload.ts`,
//! `ts/polls/enqueuePollTerminateForSend.preload.ts` and
//! `ts/jobs/helpers/sendPollVote.preload.ts`).

use presage::proto::data_message::{PollCreate, PollTerminate, PollVote as WirePollVote, ProtocolVersion};
use signal_store::{Notice, Poll, PollVote, VoteSource};

use super::*;

/// Votes and ends that came before their poll, kept (in memory, as
/// Desktop's `pollVoteCache`) until it arrives. Oldest go first.
const MAX_EARLY_POLL_EVENTS: usize = 200;

impl Actor {
    /// Whether a vote or end from `voter` (`None`: us) may change `poll`
    /// (Desktop's `doesVoteModifierMatchMessage`): ours always; anyone
    /// else's only if they belong in the poll's chat (a member of the
    /// group, or the other person in a 1:1 chat), and not if the chat or
    /// they are blocked. Desktop also checks that one of our polls was sent
    /// to them; we don't keep per-recipient send states, so membership
    /// stands in for it.
    async fn may_change_poll(&self, poll: &Message, voter: Option<&str>) -> bool {
        let Some(voter) = voter else { return true };
        if self.is_blocked(&poll.conversation_id).await || self.is_blocked(&ConversationId::direct(voter)).await {
            return false;
        }
        match poll.conversation_id.as_group() {
            Some(_) => self
                .db
                .conversation(&poll.conversation_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|c| c.members.iter().any(|m| m == voter)),
            None => poll.conversation_id == ConversationId::direct(voter),
        }
    }

    fn keep_early_poll_event(&self, action: Action) {
        let mut early = self.early_polls.borrow_mut();
        if early.len() >= MAX_EARLY_POLL_EVENTS {
            early.pop_front();
        }
        early.push_back(action);
    }

    /// A poll arrived: applies the votes and ends that came before it
    /// (Desktop's `drainCachedVotesForMessage`).
    /// Only ones from the poll's chat count, so a vote sent in another
    /// chat can't land on a poll that shares its author and timestamp.
    pub(super) async fn apply_early_poll_events(
        self: &Rc<Self>,
        chat: &ConversationId,
        author: Option<&str>,
        sent_at: i64,
    ) {
        let matching: Vec<Action> = {
            let mut early = self.early_polls.borrow_mut();
            let (matching, rest): (std::collections::VecDeque<Action>, _) = early.drain(..).partition(|action| match action {
                Action::PollVote { conversation, target_author, target_sent_at, .. } => {
                    conversation == chat && target_author.as_deref() == author && *target_sent_at == sent_at
                }
                Action::PollEnd { conversation, by, target_sent_at, .. } => {
                    conversation == chat && by.as_deref() == author && *target_sent_at == sent_at
                }
                _ => false,
            });
            *early = rest;
            Vec::from(matching)
        };
        for action in matching {
            match action {
                Action::PollVote { .. } => self.apply_poll_vote(action).await,
                Action::PollEnd { .. } => self.apply_poll_end(action).await,
                _ => {}
            }
        }
    }

    /// Someone's vote, or ours from another device (Desktop's
    /// `onPollVote` and `handlePollVote`).
    pub(super) async fn apply_poll_vote(self: &Rc<Self>, action: Action) {
        let Action::PollVote { conversation, voter, target_author, target_sent_at, options, vote_count, sent_at } = &action else {
            return;
        };
        let Some(message) = self.db.find_poll(target_author.as_deref(), *target_sent_at).await.ok().flatten() else {
            debug!("no poll for this vote yet");
            self.keep_early_poll_event(action);
            return;
        };
        // It must come in the poll's own chat, as a real one does.
        if message.conversation_id != *conversation || !self.may_change_poll(&message, voter.as_deref()).await {
            debug!("ignoring a vote from outside the poll's chat");
            return;
        }
        let source = if voter.is_none() { VoteSource::Sync } else { VoteSource::Other };
        let vote = PollVote { author: voter.clone(), options: options.clone(), vote_count: *vote_count, sent_at: *sent_at, pending: false };
        match self.db.update_poll(message.id, |poll| poll.apply_vote(vote, source)).await {
            Ok(true) => {
                self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id: message.id });
                // Desktop tells a poll's author when someone else votes.
                if message.direction == Direction::Outgoing
                    && let Some(voter) = voter
                {
                    let sender = self.name_of(voter).await.unwrap_or_else(|| short_id(voter));
                    self.emit(Event::PollActivity {
                        conversation: message.conversation_id.clone(),
                        message_id: message.id,
                        sender,
                        ended: false,
                    });
                }
            }
            // Ended, an option it doesn't have, or older than their last.
            Ok(false) => debug!("poll vote not applied"),
            Err(error) => self.error("failed to store poll vote", error),
        }
    }

    /// A poll's creator ended it (Desktop's `onPollTerminate` and
    /// `handlePollTerminate`). Only its creator can: the poll is looked up
    /// with them as its author.
    pub(super) async fn apply_poll_end(self: &Rc<Self>, action: Action) {
        let Action::PollEnd { conversation, by, target_sent_at, sent_at } = &action else { return };
        let Some(message) = self.db.find_poll(by.as_deref(), *target_sent_at).await.ok().flatten() else {
            debug!("no poll for this end yet");
            self.keep_early_poll_event(action);
            return;
        };
        if message.conversation_id != *conversation || !self.may_change_poll(&message, by.as_deref()).await {
            debug!("ignoring a poll end from outside the poll's chat");
            return;
        }
        if self.end_poll_locally(&message, *sent_at).await
            && let Some(by) = by
        {
            let sender = self.name_of(by).await.unwrap_or_else(|| short_id(by));
            self.emit(Event::PollActivity { conversation: message.conversation_id.clone(), message_id: message.id, sender, ended: true });
        }
    }

    /// Ends `message`'s poll and adds Desktop's "… ended the poll" notice
    /// at `at`; `false` if it had already ended.
    async fn end_poll_locally(&self, message: &Message, at: i64) -> bool {
        match self.db.update_poll(message.id, Poll::end).await {
            Ok(true) => {
                let question = message.poll.as_ref().map(|p| p.question.clone()).unwrap_or_default();
                let notice = Notice::PollEnded { by: message.author.clone(), question, sent_at: message.sent_at };
                self.notice(&message.conversation_id, at, notice).await;
                self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id: message.id });
                true
            }
            Ok(false) => false,
            Err(error) => {
                self.error("failed to end poll", error);
                false
            }
        }
    }

    /// Desktop's "New poll" send: a message with only the poll (no body),
    /// needing a client that knows polls.
    pub(super) async fn send_poll(self: Rc<Self>, conversation: ConversationId, question: String, options: Vec<String>, allow_multiple: bool) {
        if conversation.as_group().is_none() && !crate::POLLS_IN_DIRECT_CHATS {
            return self.error("cannot send poll", "polls can only be made in groups");
        }
        if let Some(error) = signal_store::poll_send_error(&question, &options) {
            return self.emit(Event::Error(error.into()));
        }
        let question = question.trim().to_owned();
        let options: Vec<String> = options.iter().map(|o| o.trim().to_owned()).filter(|o| !o.is_empty()).collect();
        let Some((id, mut data, timestamp)) = self.store_outgoing(&conversation, question.clone(), None, Vec::new(), None).await
        else {
            return;
        };
        if let Err(error) = self.db.set_poll(id, &Poll::new(question, options, allow_multiple)).await {
            return self.error("failed to store poll", error);
        }
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
        self.emit(Event::ConversationsChanged);
        if let Ok(Some(message)) = self.db.message(id).await {
            poll_data(&mut data, &message);
        }
        self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp, message: Some(id) });
    }

    /// Our vote (Desktop's `enqueuePollVoteForSend`): it shows at once as
    /// pending, counts once sent, and goes if it can't be sent.
    pub(super) async fn vote_in_poll(&self, message_id: i64, options: Vec<u32>) {
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        let timestamp = self.next_timestamp();
        let mut vote_count = 0;
        let applied = self
            .db
            .update_poll(message_id, |poll| {
                vote_count = poll.next_vote_count();
                let vote = PollVote { author: None, options: options.clone(), vote_count, sent_at: timestamp, pending: true };
                poll.apply_vote(vote, VoteSource::ThisDevice)
            })
            .await;
        match applied {
            Ok(true) => {}
            Ok(false) => return debug!("vote not accepted (ended, or not its options)"),
            Err(error) => return self.error("failed to store poll vote", error),
        }
        self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id });
        let author = message.author.clone().unwrap_or_else(|| self.self_aci());
        let Some(author) = Aci::parse_from_service_id_string(&author) else { return };
        let data = DataMessage {
            timestamp: Some(timestamp as u64),
            poll_vote: Some(WirePollVote {
                target_author_aci_binary: Some(author.service_id_binary()),
                target_sent_timestamp: Some(message.sent_at as u64),
                option_indexes: options,
                vote_count: Some(vote_count),
            }),
            group_v2: self.group_context(&message.conversation_id).await,
            ..Default::default()
        };
        self.enqueue(Outbound::PollVote {
            conversation: message.conversation_id,
            body: data.into(),
            timestamp: timestamp as u64,
            message: message_id,
            vote_count,
        });
    }

    /// Our vote finished sending (Desktop's `markOutgoingPollVoteSent` /
    /// `markOutgoingPollVoteFailed`).
    pub(super) async fn finish_poll_vote(&self, conversation: &ConversationId, message_id: i64, vote_count: u32, sent: bool) {
        match self.db.update_poll(message_id, |poll| poll.finish_our_vote(vote_count, sent)).await {
            Ok(true) => self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id }),
            Ok(false) => {}
            Err(error) => self.error("failed to store poll vote", error),
        }
    }

    /// Ends our poll for everyone (Desktop's `enqueuePollTerminateForSend`;
    /// "End poll" is offered only on our own polls).
    pub(super) async fn end_poll(&self, message_id: i64) {
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        if message.direction != Direction::Outgoing || message.poll.as_ref().is_none_or(|p| p.ended) {
            return;
        }
        let timestamp = self.next_timestamp();
        if !self.end_poll_locally(&message, timestamp).await {
            return;
        }
        let data = DataMessage {
            timestamp: Some(timestamp as u64),
            poll_terminate: Some(PollTerminate { target_sent_timestamp: Some(message.sent_at as u64) }),
            group_v2: self.group_context(&message.conversation_id).await,
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation: message.conversation_id, body: data.into(), timestamp: timestamp as u64, message: None });
    }
}

/// Makes `data` carry `message`'s poll in place of a body (Desktop's
/// `SendMessage` with `pollCreate`, which also requires the POLLS protocol
/// version).
pub(super) fn poll_data(data: &mut DataMessage, message: &Message) {
    let Some(poll) = &message.poll else { return };
    data.body = None;
    data.body_ranges.clear();
    data.poll_create = Some(PollCreate {
        question: Some(poll.question.clone()),
        allow_multiple: Some(poll.allow_multiple),
        options: poll.options.clone(),
    });
    data.required_protocol_version = Some(data.required_protocol_version.unwrap_or(0).max(ProtocolVersion::Polls as u32));
}
