//! Polls, as Desktop applies them (`ts/messageModifiers/Polls.preload.ts`)
//! and sends them (`ts/util/enqueuePollCreateForSend.dom.ts`,
//! `ts/polls/enqueuePollVoteForSend.preload.ts`,
//! `ts/polls/enqueuePollTerminateForSend.preload.ts`,
//! `ts/jobs/helpers/sendPollVote.preload.ts` and its job queue,
//! `ts/jobs/conversationJobQueue.preload.ts`).

use presage::proto::data_message::{PollCreate, PollTerminate, PollVote as WirePollVote, ProtocolVersion};
use signal_store::{Notice, Poll, PollVote, VoteSource};

use super::*;
use super::early::Outcome;

/// How long Desktop's job queue keeps retrying a send (`MAX_RETRY_TIME`, a
/// day from the vote).
const VOTE_RETRY_MILLIS: i64 = 24 * 60 * 60 * 1000;
/// Desktop's attempts in that day (`exponentialBackoffMaxAttempts`).
const VOTE_MAX_ATTEMPTS: u32 = 111;

/// How long to wait before attempt `attempt` (from 1), as Desktop's
/// `exponentialBackoffSleepTime`: none, then 190 ms, then 1.9 times longer
/// each time, at most 15 minutes.
pub(super) fn vote_backoff(attempt: u32) -> Duration {
    let millis = match attempt {
        0 | 1 => 0.,
        2 => 190.,
        n => (190. / 1.9) * 1.9f64.powi(n as i32 - 1),
    };
    Duration::from_millis(millis.min(15. * 60. * 1000.) as u64)
}

/// Whether a vote may change a poll (`Actor::may_change_poll`).
#[derive(Debug, PartialEq, Eq)]
enum Voter {
    Allowed,
    /// Our poll hasn't reached them, as far as we know yet: kept until it
    /// does (a vote can come back before a group send has finished).
    NotReached,
    Refused,
}

impl Actor {
    /// Whether a vote or end from `voter` (`None`: us) may change `poll`
    /// (Desktop's `doesVoteModifierMatchMessage`): ours always; anyone
    /// else's only if they belong in the poll's chat (a member of the
    /// group, or the other person in a 1:1 chat), and not if the chat or
    /// they are blocked. In our poll, only someone it reached may vote; for
    /// our polls from before we kept who they reached, membership stands in.
    async fn may_change_poll(&self, poll: &Message, voter: Option<&str>) -> Voter {
        let Some(voter) = voter else { return Voter::Allowed };
        if self.is_blocked(&poll.conversation_id).await || self.is_blocked(&ConversationId::direct(voter)).await {
            return Voter::Refused;
        }
        let member = match poll.conversation_id.as_group() {
            Some(_) => self
                .db
                .conversation(&poll.conversation_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|c| c.members.iter().any(|m| m == voter)),
            None => poll.conversation_id == ConversationId::direct(voter),
        };
        if !member {
            return Voter::Refused;
        }
        let reached = poll.poll.as_ref().and_then(|p| p.reached(voter));
        match (poll.direction, reached) {
            (Direction::Outgoing, Some(false)) => Voter::NotReached,
            _ => Voter::Allowed,
        }
    }

    /// Someone's vote, or ours from another device (Desktop's
    /// `onPollVote` and `handlePollVote`).
    /// One that comes before its poll is kept (Desktop's `pollVoteCache`,
    /// here `early.rs`).
    pub(super) async fn apply_poll_vote(self: &Rc<Self>, action: &Action) -> Outcome {
        let Action::PollVote { conversation, voter, target_author, target_sent_at, options, vote_count, sent_at } = action else {
            return Outcome::Refused;
        };
        let message = match self.db.find_poll(target_author.as_deref(), *target_sent_at).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                debug!("no poll for this vote yet");
                return Outcome::Missing;
            }
            Err(error) => {
                self.error("failed to look up poll", error);
                return Outcome::Failed;
            }
        };
        // It must come in the poll's own chat, as a real one does.
        if message.conversation_id != *conversation {
            debug!("ignoring a vote from outside the poll's chat");
            return Outcome::Refused;
        }
        match self.may_change_poll(&message, voter.as_deref()).await {
            Voter::Allowed => {}
            // Kept with the events that came before their message, and
            // tried again when a send reaches them (`poll_reached`).
            Voter::NotReached => {
                debug!("vote from someone our poll hasn't reached (yet)");
                return Outcome::Missing;
            }
            Voter::Refused => {
                debug!("ignoring a vote from outside the poll's chat");
                return Outcome::Refused;
            }
        }
        let source = if voter.is_none() { VoteSource::Sync } else { VoteSource::Other };
        let vote = PollVote { author: voter.clone(), options: options.clone(), vote_count: *vote_count, sent_at: *sent_at, ..Default::default() };
        // Someone else's vote in our poll is unread until the chat is
        // (Desktop's `hasUnreadPollVotes`).
        let theirs_in_ours = message.direction == Direction::Outgoing && voter.is_some();
        let applied = if theirs_in_ours {
            self.db.update_poll_unread(message.id, |poll| poll.apply_vote(vote, source)).await
        } else {
            self.db.update_poll(message.id, |poll| poll.apply_vote(vote, source)).await
        };
        match applied {
            Ok(true) => {
                self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id: message.id });
                // Desktop tells a poll's author when someone else votes.
                if theirs_in_ours && let Some(voter) = voter {
                    self.emit(Event::ConversationsChanged);
                    let sender = self.name_of(voter).await.unwrap_or_else(|| short_id(voter));
                    self.emit(Event::PollActivity {
                        conversation: message.conversation_id.clone(),
                        message_id: message.id,
                        sender,
                        ended: false,
                    });
                }
                Outcome::Applied
            }
            // Ended, an option it doesn't have, or older than their last.
            Ok(false) => {
                debug!("poll vote not applied");
                Outcome::Refused
            }
            Err(error) => {
                self.error("failed to store poll vote", error);
                Outcome::Failed
            }
        }
    }

    /// A poll's creator ended it (Desktop's `onPollTerminate` and
    /// `handlePollTerminate`). Only its creator can: the poll is looked up
    /// with them as its author.
    pub(super) async fn apply_poll_end(self: &Rc<Self>, manager: &SignalManager, action: &Action) -> Outcome {
        let Action::PollEnd { conversation, by, target_sent_at, sent_at } = action else { return Outcome::Refused };
        let message = match self.db.find_poll(by.as_deref(), *target_sent_at).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                debug!("no poll for this end yet");
                return Outcome::Missing;
            }
            Err(error) => {
                self.error("failed to look up poll", error);
                return Outcome::Failed;
            }
        };
        if message.conversation_id != *conversation || self.may_change_poll(&message, by.as_deref()).await != Voter::Allowed {
            debug!("ignoring a poll end from outside the poll's chat");
            return Outcome::Refused;
        }
        if self.end_poll_locally(&message, *sent_at).await
            && let Some(by) = by
        {
            // A read sync for its notice may have come first.
            Box::pin(self.apply_early_for(manager, Some(by), *sent_at)).await;
            let sender = self.name_of(by).await.unwrap_or_else(|| short_id(by));
            self.emit(Event::PollActivity { conversation: message.conversation_id.clone(), message_id: message.id, sender, ended: true });
        }
        Outcome::Applied
    }

    /// Ends `message`'s poll and adds Desktop's "… ended the poll" notice
    /// at `at`; `false` if it had already ended. Someone else's end is
    /// unread until the chat is, and either becomes the chat's preview
    /// (Desktop's `addPollTerminateNotification`).
    async fn end_poll_locally(&self, message: &Message, at: i64) -> bool {
        match self.db.update_poll(message.id, Poll::end).await {
            Ok(true) => {
                let question = message.poll.as_ref().map(|p| p.question.clone()).unwrap_or_default();
                let notice = Notice::PollEnded { by: message.author.clone(), question, sent_at: message.sent_at };
                let added = if message.author.is_some() {
                    self.db.insert_unread_notice(&message.conversation_id, at, &notice).await
                } else {
                    self.db.insert_notice(&message.conversation_id, at, &notice).await
                };
                match added {
                    Ok(Some(id)) => {
                        self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id: id })
                    }
                    Ok(None) => {}
                    Err(error) => self.error("failed to add notice", error),
                }
                self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id: message.id });
                self.emit(Event::ConversationsChanged);
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
        // Desktop's `enqueuePollCreateForSend`.
        if conversation.as_group().is_none() && !crate::remote_config().poll_send_1to1() {
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
        // Nobody may vote until it reaches them.
        let poll = Poll { reached: Some(Default::default()), ..Poll::new(question, options, allow_multiple) };
        if let Err(error) = self.db.set_poll(id, &poll).await {
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
    /// pending, addressed to everyone else in the chat, and counts once it
    /// has reached them all.
    pub(super) async fn vote_in_poll(&self, message_id: i64, options: Vec<u32>) {
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        let recipients = self.vote_recipients(&message.conversation_id).await;
        let timestamp = self.next_timestamp();
        let mut vote_count = 0;
        let applied = self
            .db
            .update_poll(message_id, |poll| {
                vote_count = poll.next_vote_count();
                let vote = PollVote {
                    author: None,
                    options: options.clone(),
                    vote_count,
                    sent_at: timestamp,
                    pending: true,
                    sends: recipients.iter().map(|r| (r.clone(), false)).collect(),
                };
                poll.apply_vote(vote, VoteSource::ThisDevice)
            })
            .await;
        match applied {
            Ok(true) => {}
            Ok(false) => return debug!("vote not accepted (ended, or not its options)"),
            Err(error) => return self.error("failed to store poll vote", error),
        }
        self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id });
        self.enqueue(Outbound::PollVote { message: message_id, vote_count, attempt: 1 });
    }

    /// Who our vote goes to: everyone else in the chat (Desktop's
    /// `getMemberConversationIds`, without us).
    async fn vote_recipients(&self, conversation: &ConversationId) -> Vec<String> {
        let me = self.self_aci();
        match conversation.as_group() {
            Some(_) => self
                .db
                .conversation(conversation)
                .await
                .ok()
                .flatten()
                .map(|c| c.members.into_iter().filter(|m| *m != me).collect())
                .unwrap_or_default(),
            None => conversation.as_direct().filter(|other| *other != me).map(|o| vec![o.to_owned()]).unwrap_or_default(),
        }
    }

    /// Sends our vote numbered `vote_count` in poll `message_id` to whoever
    /// it hasn't reached (Desktop's `sendPollVote` job), and tries again
    /// later if it missed anyone, as Desktop's job queue does: with
    /// backoff, for a day from the vote, waiting longer when rate limited,
    /// and stopping when the server refuses it. Given up, it counts if it
    /// reached anyone and goes if not. `None` for `manager`: not connected,
    /// which is an attempt that missed everyone.
    pub(super) async fn send_poll_vote(
        self: &Rc<Self>,
        manager: Option<&mut SignalManager>,
        message_id: i64,
        vote_count: u32,
        attempt: u32,
    ) {
        // Deleted, or no longer a poll: nothing to send.
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        let Some(poll) = &message.poll else { return };
        // A newer vote replaced it (and has its own send), or it's done.
        let Some(mut vote) = poll.our_pending_vote().filter(|v| v.vote_count == vote_count).cloned() else { return };
        let conversation = message.conversation_id.clone();
        // One kept from before recipients were: addressed now.
        if vote.sends.is_empty() {
            let recipients = self.vote_recipients(&conversation).await;
            if let Err(error) = self.db.update_poll(message_id, |poll| poll.address_our_vote(vote_count, &recipients)).await {
                return self.error("failed to store poll vote", error);
            }
            vote.sends = recipients.into_iter().map(|r| (r, false)).collect();
        }
        // Desktop's `shouldSendToConversation`: not to a group we left or a
        // chat we blocked.
        let chat = self.db.conversation(&conversation).await.ok().flatten();
        if chat.as_ref().is_none_or(|c| c.left || c.blocked) {
            debug!("not sending a vote to a chat we left or blocked");
            return self.give_up_vote(&conversation, message_id, vote_count).await;
        }
        let delivery = match manager {
            Some(manager) => {
                let author = message.author.clone().unwrap_or_else(|| self.self_aci());
                let Some(author) = Aci::parse_from_service_id_string(&author) else { return };
                let data = DataMessage {
                    timestamp: Some(vote.sent_at as u64),
                    poll_vote: Some(WirePollVote {
                        target_author_aci_binary: Some(author.service_id_binary()),
                        target_sent_timestamp: Some(message.sent_at as u64),
                        option_indexes: vote.options.clone(),
                        vote_count: Some(vote_count),
                    }),
                    group_v2: self.group_context(&conversation).await,
                    ..Default::default()
                };
                let unsent = vote.unsent();
                self.deliver_to(manager, &conversation, data.into(), vote.sent_at as u64, Some(&unsent)).await
            }
            None => Delivery { error: Some("not connected".into()), ..Default::default() },
        };
        // A 428: the captcha dialog asks; the vote goes again on its schedule.
        if let Some(token) = &delivery.challenge {
            self.note_challenge(token).await;
        }
        let recorded = self
            .db
            .update_poll(message_id, |poll| poll.record_vote_sends(vote_count, &delivery.reached, &delivery.unregistered))
            .await;
        match recorded {
            Ok(true) => self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id }),
            Ok(false) => {}
            Err(error) => return self.error("failed to store poll vote", error),
        }
        let still_pending = self
            .db
            .message(message_id)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.poll)
            .is_some_and(|p| p.our_pending_vote().is_some_and(|v| v.vote_count == vote_count));
        if !still_pending {
            return;
        }
        let missed = delivery.failed.len();
        let why = delivery.error.clone().or_else(|| delivery.failed.first().map(|(_, e)| e.clone())).unwrap_or_default();
        let out_of_time = now_millis() - vote.sent_at > VOTE_RETRY_MILLIS || attempt >= VOTE_MAX_ATTEMPTS;
        if delivery.rejected || out_of_time {
            warn!(attempt, missed, rejected = delivery.rejected, %why, "giving up sending a poll vote");
            return self.give_up_vote(&conversation, message_id, vote_count).await;
        }
        let wait = vote_backoff(attempt + 1).max(delivery.retry_after.unwrap_or_default());
        info!(attempt, missed, ?wait, %why, "poll vote not sent to everyone; trying again");
        let this = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(wait).await;
            this.enqueue(Outbound::PollVote { message: message_id, vote_count, attempt: attempt + 1 });
        });
    }

    /// Stops sending our vote (Desktop's `markOutgoingPollVoteFailed`).
    async fn give_up_vote(&self, conversation: &ConversationId, message_id: i64, vote_count: u32) {
        match self.db.update_poll(message_id, |poll| poll.give_up_vote(vote_count)).await {
            Ok(true) => self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id }),
            Ok(false) => {}
            Err(error) => self.error("failed to store poll vote", error),
        }
    }

    /// Queues our votes that were still sending when the app last quit (in
    /// Desktop they're jobs, kept until they finish).
    pub(super) async fn resume_poll_votes(&self) {
        let polls = match self.db.polls_with_pending_votes().await {
            Ok(polls) => polls,
            Err(error) => return self.error("failed to find poll votes to send", error),
        };
        for message in polls {
            if let Some(vote) = message.poll.as_ref().and_then(|p| p.our_pending_vote()) {
                self.enqueue(Outbound::PollVote { message: message.id, vote_count: vote.vote_count, attempt: 1 });
            }
        }
    }

    /// Our message `message_id` reached `reached`: if it's a poll, they may
    /// vote in it now, and their votes that came first count.
    pub(super) async fn poll_reached(self: &Rc<Self>, manager: &SignalManager, message_id: i64, reached: &[String]) {
        if reached.is_empty() {
            return;
        }
        let who = reached.to_vec();
        match self.db.update_poll(message_id, |poll| poll.record_reached(who)).await {
            Ok(true) => {
                if let Ok(Some(message)) = self.db.message(message_id).await {
                    self.apply_early_for(manager, None, message.sent_at).await;
                }
            }
            Ok(false) => {}
            Err(error) => self.error("failed to store who the poll reached", error),
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
