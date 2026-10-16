//! Group calls, as Desktop makes them: RingRTC's group call client through
//! Signal's calling server. We hand it a membership proof and the members
//! when it asks, carry its HTTP requests and opaque call messages, ring the
//! group when we start a call no one's in, and tell the group ("… started a
//! group call").
//!
//! Each call is kept in the call history as Desktop keeps it
//! (`ts/util/callDisposition.preload.ts`, driven from
//! `ts/services/calling.preload.ts`): rung, missed, declined, joined or
//! accepted, with our other devices told when we accept one.

use std::sync::OnceLock;

use presage::libsignal_service::content::ContentBody;
use presage::proto::{CallMessage, call_message};
use signal_calls::{ClientId, GroupEvent, Participant, RingCancelReason};
use signal_store::{CallHistoryEvent, GroupCallRecord, GroupCallStatus};

use super::*;
use crate::{CallParticipant, CallStatus, CallUpdate};

/// Desktop rings groups of up to this many members (`maxGroupCallRingSize`);
/// bigger ones are only told.
const MAX_RING_SIZE: usize = 16;

/// Where a group call's event comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HistorySource {
    /// Here: our other devices hear of an accepted call.
    Local,
    /// Someone's `groupCallUpdate`: recorded only if the call is new to us
    /// (Desktop's `updateCallHistoryForGroupCallOnPeek`).
    IfNew,
    /// Another of our devices' call event sync, which isn't echoed back.
    OtherDevice,
}

/// The group call we're in, joining, or being rung for.
pub(super) struct GroupCall {
    conversation: ConversationId,
    key: [u8; 32],
    client_id: Option<ClientId>,
    status: CallStatus,
    video: bool,
    participants: Vec<Participant>,
    /// A ring we haven't answered: the group's identifier, the ring, who.
    ring: Option<(Vec<u8>, i64, String)>,
    /// We started it: ring the group if no one's there.
    starter: bool,
    /// The call's era, from the calling server.
    era: Option<String>,
    /// Who started it (their ACI), from the calling server.
    creator: Option<String>,
    /// What our joining last recorded in the call history.
    recorded: Option<CallHistoryEvent>,
    /// Our demux id, once we're in.
    our_demux: Option<u32>,
    /// Whose hands are up, in the order they went up.
    raised_hands: Vec<u32>,
    /// Our microphone is on (a remote mute only mutes an open one).
    audio: bool,
    /// The participants on screen, from the UI (`None`: all of them), and
    /// the one shown large.
    visible: Option<(Vec<u32>, Option<u32>)>,
    rang: bool,
    announced: bool,
    joined: bool,
    sharing: bool,
    camera_before_sharing: bool,
    /// A call link's call (`call_links.rs`); `key` is unused then.
    link: Option<super::call_links::LinkCallState>,
}

impl GroupCall {
    fn new(conversation: ConversationId, key: [u8; 32], status: CallStatus) -> Self {
        Self {
            conversation,
            key,
            client_id: None,
            status,
            video: true,
            participants: Vec::new(),
            ring: None,
            starter: false,
            era: None,
            creator: None,
            recorded: None,
            our_demux: None,
            raised_hands: Vec::new(),
            audio: true,
            visible: None,
            rang: false,
            announced: false,
            joined: false,
            sharing: false,
            camera_before_sharing: false,
            link: None,
        }
    }

    /// The call's record in the history, once the calling server has told
    /// us its era and who started it (Desktop's `getGroupCallMeta`).
    fn record(&self, me: &str) -> Option<GroupCallRecord> {
        let (era, creator) = (self.era.clone()?, self.creator.clone()?);
        let incoming = creator != me;
        Some(GroupCallRecord {
            call_id: signal_calls::call_id_from_era(&era),
            era: Some(era),
            by: incoming.then_some(creator),
            incoming,
            at: now_millis(),
        })
    }
}

impl GroupCall {
    pub(super) fn update(&self) -> CallUpdate {
        // Desktop's order: whoever spoke last first, then who came first.
        let mut participants = self.participants.clone();
        participants.sort_by(|a, b| b.speaker_time.cmp(&a.speaker_time).then(a.added_time.cmp(&b.added_time)));
        CallUpdate {
            conversation: self.conversation.clone(),
            status: self.status.clone(),
            video: self.video,
            incoming: self.ring.is_some(),
            remote_video: false,
            remote_audio: true,
            sharing: self.sharing,
            remote_sharing: participants.iter().any(|p| p.presenting),
            group: true,
            participants: participants
                .iter()
                .map(|p| CallParticipant {
                    aci: aci_of(&p.user_id).unwrap_or_default(),
                    demux_id: p.demux_id,
                    video: !p.video_muted,
                    audio: !p.audio_muted,
                    presenting: p.presenting,
                })
                .collect(),
            ringer: self.ring.as_ref().map(|(_, _, ringer)| ringer.clone()),
            raised_hands: self.raised_hands.clone(),
            local_demux_id: self.our_demux,
            link: self.link.as_ref().map(super::call_links::LinkCallState::view),
        }
    }
}

/// An ACI's string from its 16 bytes.
fn aci_of(bytes: &[u8]) -> Option<String> {
    let bytes: [u8; 16] = bytes.try_into().ok()?;
    Some(Aci::from_uuid_bytes(bytes).service_id_string())
}

/// For the calling server, which (as Signal's other servers) has a
/// certificate from Signal's own root CA: trusting only that, as Desktop
/// does. (reqwest here has no TLS of its own.)
pub(super) fn http() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let client = crate::signal_tls().and_then(|tls| {
                Ok(reqwest::Client::builder()
                    .use_preconfigured_tls(tls)
                    .timeout(std::time::Duration::from_secs(30))
                    .build()?)
            });
            client.inspect_err(|error| warn!(error = %format!("{error:#}"), "no calling server client")).ok()
        })
        .as_ref()
}

impl Actor {
    /// The group's members, from its stored copy.
    async fn group_members(&self, key: [u8; 32]) -> Vec<Aci> {
        self.store.group(key).await.ok().flatten().map(|g| g.members.iter().map(|m| m.aci).collect()).unwrap_or_default()
    }

    /// The group call we're in, as the call history names it (Desktop's
    /// `getActiveCallIds`): its notice doesn't start disappearing yet.
    pub(super) fn active_group_call_id(&self) -> Option<(ConversationId, i64)> {
        let call = self.group_call.borrow();
        let call = call.as_ref().filter(|g| g.client_id.is_some())?;
        Some((call.conversation.clone(), signal_calls::call_id_from_era(call.era.as_deref()?)))
    }

    pub(super) fn in_any_call(&self) -> bool {
        self.call.borrow().is_some() || self.group_call.borrow().as_ref().is_some_and(|g| g.client_id.is_some())
    }

    /// Joins (or starts) a group's call.
    pub(super) fn start_group_call(&self, conversation: ConversationId, video: bool) {
        if self.in_any_call() {
            return self.error("can't call", "you're already in a call");
        }
        let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return };
        if !self.call_session() {
            return self.call_not_started();
        }
        let mut client = Err(anyhow::anyhow!("no session"));
        self.with_calls(|calls| client = calls.create_group_call(crate::groups::group_identifier(key)));
        let client_id = match client {
            Ok(id) => id,
            Err(error) => {
                self.error("can't call", format!("{error:#}"));
                return self.call_not_started();
            }
        };
        *self.group_call.borrow_mut() =
            Some(GroupCall { client_id: Some(client_id), video, starter: true, ..GroupCall::new(conversation, key, CallStatus::Connecting) });
        self.with_calls(|calls| {
            calls.set_video_enabled(video);
            calls.set_group_audio_muted(client_id, false);
            calls.set_group_video_muted(client_id, !video);
            calls.group_connect(client_id);
            calls.group_join(client_id);
        });
        self.emit_call();
    }

    /// Answers a group call's ring: joins it.
    pub(super) fn accept_group_call(&self, video: bool) -> bool {
        let Some(key) = self.group_call.borrow().as_ref().filter(|g| g.ring.is_some()).map(|g| g.key) else { return false };
        let mut client = Err(anyhow::anyhow!("no session"));
        self.with_calls(|calls| client = calls.create_group_call(crate::groups::group_identifier(key)));
        let client_id = match client {
            Ok(id) => id,
            Err(error) => {
                self.error("can't join the call", format!("{error:#}"));
                return true;
            }
        };
        if let Some(call) = self.group_call.borrow_mut().as_mut() {
            call.client_id = Some(client_id);
            call.ring = None;
            call.video = video;
            call.status = CallStatus::Connecting;
        }
        self.with_calls(|calls| {
            calls.set_video_enabled(video);
            calls.set_group_audio_muted(client_id, false);
            calls.set_group_video_muted(client_id, !video);
            calls.group_connect(client_id);
            calls.group_join(client_id);
        });
        self.emit_call();
        true
    }

    /// Declines a ring, or leaves the call. `false`: no group call.
    pub(super) async fn end_group_call(&self) -> bool {
        let Some(call) = self.group_call.borrow_mut().take() else { return false };
        match (call.client_id, &call.ring) {
            (Some(client_id), _) => {
                self.with_calls(|calls| {
                    calls.group_end(client_id);
                    calls.reset_media();
                });
                self.emit_call();
                self.left_group_call(&call).await;
            }
            (None, Some((group_id, ring_id, ringer))) => {
                let (group_id, ring_id) = (group_id.clone(), *ring_id);
                self.with_calls(|calls| calls.cancel_group_ring(group_id, ring_id, RingCancelReason::DeclinedByUser));
                self.emit_call();
                // Desktop's transition has a declined group call, though
                // its own decline leaves the ring's record as it was; ours
                // says what we did. (Other devices hear "declined on another
                // device" from RingRTC, which they count as missed.)
                let record = GroupCallRecord {
                    call_id: ring_id,
                    era: None,
                    by: (!ringer.is_empty()).then(|| ringer.clone()),
                    incoming: true,
                    at: now_millis(),
                };
                self.group_call_history(&call.conversation, record, CallHistoryEvent::Declined, false, HistorySource::Local).await;
            }
            (None, None) => self.emit_call(),
        }
        true
    }

    /// We left (or the call ended): Desktop's local state change to "not
    /// joined", which records the call as started (an accepted ring stays
    /// accepted) and starts its disappearing countdown; and, as Desktop
    /// sends on joining and on leaving, a `groupCallUpdate` to the group.
    async fn left_group_call(&self, call: &GroupCall) {
        // A call link's call is recorded by `call_links.rs`; it has no group.
        if call.link.is_some() {
            return;
        }
        if call.recorded.is_some()
            && let Some(record) = call.record(&self.self_aci())
        {
            self.group_call_history(&call.conversation, record, CallHistoryEvent::Started, false, HistorySource::Local).await;
        }
        if call.announced
            && let Some(era) = call.era.clone()
        {
            self.send_group_call_update(&call.conversation, era).await;
        }
    }

    /// Our microphone and camera in a group call (as well as the tracks).
    pub(super) fn group_media(&self, audio: Option<bool>, video: Option<bool>) {
        let Some(client_id) = self.group_call.borrow().as_ref().and_then(|g| g.client_id) else { return };
        if let (Some(on), Some(call)) = (audio, self.group_call.borrow_mut().as_mut()) {
            call.audio = on;
        }
        self.with_calls(|calls| {
            if let Some(on) = audio {
                calls.set_group_audio_muted(client_id, !on);
            }
            if let Some(on) = video {
                calls.set_group_video_muted(client_id, !on);
            }
        });
    }

    pub(super) fn group_presenting(&self, on: bool) {
        let Some(client_id) = self.group_call.borrow().as_ref().and_then(|g| g.client_id) else { return };
        self.with_calls(|calls| calls.set_group_presenting(client_id, on));
    }

    /// Who's in a group's call, for the lobby.
    pub(super) async fn peek_group_call(self: &Rc<Self>, conversation: ConversationId) {
        let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return };
        let Some(manager) = self.manager() else { return };
        if !self.call_session() {
            return;
        }
        let proof = match crate::groups::GroupSession::new(&manager, key).await {
            Ok(session) => session.call_proof().await,
            Err(error) => Err(error),
        };
        let proof = match proof {
            Ok(proof) => proof,
            Err(error) => return warn!(error = %format!("{error:#}"), "couldn't peek the group call"),
        };
        let members = crate::groups::call_members(key, &self.group_members(key).await);
        let request_id = self.next_peek.get().wrapping_add(1);
        self.next_peek.set(request_id);
        self.peeks.borrow_mut().insert(request_id, conversation);
        self.with_calls(|calls| calls.peek_group_call(request_id, proof, members));
    }

    pub(super) async fn on_group_event(self: &Rc<Self>, event: GroupEvent) {
        match event {
            GroupEvent::NeedsProof(client_id) => {
                let Some(key) = self.group_key(client_id) else { return };
                let Some(manager) = self.manager() else { return };
                let this = self.clone();
                tokio::task::spawn_local(async move {
                    let proof = match crate::groups::GroupSession::new(&manager, key).await {
                        Ok(session) => session.call_proof().await,
                        Err(error) => Err(error),
                    };
                    match proof {
                        Ok(proof) => this.with_calls(|calls| calls.set_membership_proof(client_id, proof)),
                        Err(error) => warn!(error = %format!("{error:#}"), "no group call membership proof"),
                    }
                });
            }
            GroupEvent::NeedsMembers(client_id) => {
                let Some(key) = self.group_key(client_id) else { return };
                let members = crate::groups::call_members(key, &self.group_members(key).await);
                self.with_calls(|calls| calls.set_group_members(client_id, members));
            }
            GroupEvent::Connection { client_id, connected, connecting } => {
                if let Some(call) = self.group_call.borrow_mut().as_mut().filter(|g| g.client_id == Some(client_id))
                    && !call.joined
                    && (connecting || connected)
                {
                    call.status = CallStatus::Connecting;
                }
                self.emit_call();
            }
            GroupEvent::Joined { client_id, joined, pending } => {
                let ring = {
                    let mut call = self.group_call.borrow_mut();
                    let Some(call) = call.as_mut().filter(|g| g.client_id == Some(client_id)) else { return };
                    // Waiting for a call link's admin isn't being in it.
                    if let Some(link) = call.link.as_mut() {
                        link.waiting = pending;
                    }
                    call.joined = joined.is_some() && !pending;
                    call.our_demux = joined;
                    if call.joined && !matches!(call.status, CallStatus::Connected { .. }) {
                        call.status = CallStatus::Connected { since: now_millis() };
                    }
                    call.joined && call.starter && !call.rang && call.participants.is_empty()
                };
                // Desktop rings a group it starts a call in (if it isn't big).
                if ring && let Some(key) = self.group_key(client_id) {
                    if self.group_members(key).await.len() <= MAX_RING_SIZE {
                        self.with_calls(|calls| calls.group_ring(client_id));
                    }
                    if let Some(call) = self.group_call.borrow_mut().as_mut() {
                        call.rang = true;
                    }
                }
                self.announce_group_call(client_id).await;
                self.record_join_state(client_id).await;
                self.emit_call();
            }
            GroupEvent::Participants { client_id, participants } => {
                if let Some(call) = self.group_call.borrow_mut().as_mut().filter(|g| g.client_id == Some(client_id)) {
                    call.participants = participants;
                }
                self.request_group_video(client_id);
                self.emit_call();
            }
            GroupEvent::Reactions { client_id, reactions } => {
                let reactions: Vec<_> = reactions
                    .into_iter()
                    .filter_map(|(demux_id, emoji)| Some(crate::CallReaction { aci: self.demux_aci(client_id, demux_id)?, demux_id, emoji }))
                    .collect();
                if !reactions.is_empty() {
                    self.emit(Event::CallReactions(reactions));
                }
            }
            GroupEvent::RaisedHands { client_id, hands } => {
                if let Some(call) = self.group_call.borrow_mut().as_mut().filter(|g| g.client_id == Some(client_id)) {
                    call.raised_hands = hands;
                }
                self.emit_call();
            }
            // Desktop's `setMutedBy`: only an open microphone is muted,
            // and RingRTC tells everyone who did it.
            GroupEvent::MutedBy { client_id, source } => {
                let open = self.group_call.borrow().as_ref().is_some_and(|g| g.client_id == Some(client_id) && g.audio);
                let Some(by) = self.demux_aci(client_id, source).filter(|_| open) else { return };
                self.with_calls(|calls| calls.set_group_audio_muted_remotely(client_id, source));
                if let Some(call) = self.group_call.borrow_mut().as_mut() {
                    call.audio = false;
                }
                self.emit(Event::CallMuted { by, target: None });
            }
            GroupEvent::ObservedMute { client_id, source, target } => {
                if let (Some(by), Some(target)) = (self.demux_aci(client_id, source), self.demux_aci(client_id, target)) {
                    self.emit(Event::CallMuted { by, target: Some(target) });
                }
            }
            GroupEvent::Peek { client_id, peek } => {
                let link = {
                    let mut call = self.group_call.borrow_mut();
                    let Some(call) = call.as_mut().filter(|g| g.client_id == Some(client_id)) else { return };
                    call.era = peek.era_id.clone();
                    if let Some(creator) = peek.creator.as_deref().and_then(aci_of) {
                        call.creator = Some(creator);
                    }
                    call.link.as_mut().map(|link| {
                        link.pending = peek.pending.iter().filter_map(|u| aci_of(u)).collect();
                        link.room_id.clone()
                    })
                };
                if let Some(room_id) = link {
                    self.on_link_call_peek(&room_id, &peek).await;
                    self.emit_call();
                }
                self.announce_group_call(client_id).await;
                self.record_join_state(client_id).await;
            }
            GroupEvent::PeekResult { request_id, peek } => {
                let Some(conversation) = self.peeks.borrow_mut().remove(&request_id) else { return };
                let users = peek.map(|p| p.users.iter().filter_map(|u| aci_of(u)).collect()).unwrap_or_default();
                self.emit(Event::GroupPeek { conversation, users });
            }
            GroupEvent::Ended { client_id, reason } => {
                let ended = {
                    let mut call = self.group_call.borrow_mut();
                    if call.as_ref().is_some_and(|g| g.client_id == Some(client_id)) { call.take() } else { None }
                };
                // Desktop tells you when a call link's admin turned you away.
                if ended.as_ref().is_some_and(|g| g.link.is_some()) {
                    match reason {
                        signal_calls::GroupEnd::Denied => self.emit(Event::CallLinkRefused { removed: false }),
                        signal_calls::GroupEnd::Removed => self.emit(Event::CallLinkRefused { removed: true }),
                        signal_calls::GroupEnd::Other | signal_calls::GroupEnd::Full => {}
                    }
                }
                if let Some(call) = ended {
                    self.with_calls(|calls| {
                        calls.group_end(client_id);
                        calls.reset_media();
                    });
                    self.emit_call();
                    self.left_group_call(&call).await;
                }
            }
            GroupEvent::Ring { group_id, ring_id, sender, ring } => self.on_group_ring(group_id, ring_id, sender, ring).await,
        }
    }

    /// A ring, as Desktop's `handleGroupCallRingUpdate` takes it: one we
    /// show is ringing in the call history, one we can't (we're busy) is
    /// a call someone started; when it's over, it was accepted on another
    /// of our devices or missed.
    async fn on_group_ring(&self, group_id: Vec<u8>, ring_id: i64, sender: Vec<u8>, ring: signal_calls::Ring) {
        let Some(manager) = self.manager() else { return };
        let Some(key) = self.group_for_id(&manager, &group_id).await else { return };
        let conversation = ConversationId::group(&key);
        if self.is_blocked(&conversation).await {
            return;
        }
        let ringer = aci_of(&sender);
        let record = GroupCallRecord {
            call_id: ring_id,
            era: None,
            by: ringer.clone(),
            incoming: ringer.as_deref() != Some(self.self_aci().as_str()),
            at: now_millis(),
        };
        let event = match ring {
            signal_calls::Ring::Requested => {
                if self.in_any_call() || self.group_call.borrow().is_some() {
                    self.with_calls(|calls| calls.cancel_group_ring(group_id, ring_id, RingCancelReason::Busy));
                    CallHistoryEvent::Started
                } else {
                    *self.group_call.borrow_mut() = Some(GroupCall {
                        ring: Some((group_id, ring_id, ringer.unwrap_or_default())),
                        ..GroupCall::new(conversation.clone(), key, CallStatus::Ringing)
                    });
                    CallHistoryEvent::Ringing
                }
            }
            signal_calls::Ring::AcceptedElsewhere | signal_calls::Ring::Over => {
                let mut call = self.group_call.borrow_mut();
                if call.as_ref().is_some_and(|g| g.client_id.is_none() && g.ring.as_ref().is_some_and(|r| r.1 == ring_id)) {
                    call.take();
                }
                if ring == signal_calls::Ring::AcceptedElsewhere { CallHistoryEvent::Accepted } else { CallHistoryEvent::Missed }
            }
        };
        self.emit_call();
        // Accepted on another device: that device already told the others.
        let source = if event == CallHistoryEvent::Accepted { HistorySource::OtherDevice } else { HistorySource::Local };
        self.group_call_history(&conversation, record, event, false, source).await;
    }

    /// Desktop's `updateCallHistoryForGroupCallOnLocalChanged`: in the
    /// call, once we know its era and who started it, joining someone's
    /// call accepts it, and joining ours has it ringing (Desktop's
    /// `getLocalCallEventFromJoinState`). Each change is recorded once.
    async fn record_join_state(&self, client_id: ClientId) {
        let me = self.self_aci();
        let (conversation, record, event) = {
            let mut call = self.group_call.borrow_mut();
            let Some(call) = call.as_mut().filter(|g| g.client_id == Some(client_id) && g.joined && g.link.is_none()) else {
                return;
            };
            let Some(record) = call.record(&me) else { return };
            let event = if record.incoming { CallHistoryEvent::Accepted } else { CallHistoryEvent::Ringing };
            if call.recorded == Some(event) {
                return;
            }
            call.recorded = Some(event);
            (call.conversation.clone(), record, event)
        };
        self.group_call_history(&conversation, record, event, true, HistorySource::Local).await;
    }

    /// Applies an event to a group call's record (Desktop's
    /// `updateCallHistoryFromLocalEvent`), and tells our other devices once
    /// we've accepted it: of a group call's statuses, Desktop syncs only
    /// that one (`statusToProto`; the declined one it would send, other
    /// devices refuse for a group call).
    pub(super) async fn group_call_history(
        &self,
        conversation: &ConversationId,
        record: GroupCallRecord,
        event: CallHistoryEvent,
        active: bool,
        source: HistorySource,
    ) {
        let only_new = source == HistorySource::IfNew;
        let change = match self.db.apply_group_call_event(conversation, &record, event, active, only_new).await {
            Ok(Some(change)) => change,
            Ok(None) => return,
            Err(error) => return self.error("failed to record the call", error),
        };
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: change.id });
        self.emit(Event::ConversationsChanged);
        // A call notice may have started disappearing.
        self.expiry.notify_one();
        if source == HistorySource::Local
            && change.status == GroupCallStatus::Accepted
            && change.previous != Some(GroupCallStatus::Accepted)
            && let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok())
        {
            self.sync_group_call_event(key, record.call_id, change.sent_at, change.incoming);
        }
    }

    /// Desktop's call event sync for a group call we accepted
    /// (`getProtoForCallHistory`): the group's identifier, the call id and
    /// its time.
    fn sync_group_call_event(&self, key: [u8; 32], call_id: i64, at: i64, incoming: bool) {
        use presage::proto::sync_message::call_event::{Direction, Event as CallEventKind, Type};
        let event = presage::proto::sync_message::CallEvent {
            conversation_id: Some(crate::groups::group_identifier(key)),
            call_id: Some(call_id as u64),
            timestamp: Some(at as u64),
            r#type: Some(Type::GroupCall.into()),
            direction: Some(if incoming { Direction::Incoming } else { Direction::Outgoing }.into()),
            event: Some(CallEventKind::Accepted.into()),
        };
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::CallEvent(event)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Desktop's `groupCallUpdate`, sent on joining (once we know the
    /// call's era), so the group sees "… started a group call" (once per
    /// era) and looks for the call.
    async fn announce_group_call(&self, client_id: ClientId) {
        let (conversation, era, link) = {
            let mut call = self.group_call.borrow_mut();
            let Some(call) = call.as_mut().filter(|g| g.client_id == Some(client_id) && g.joined && !g.announced) else {
                return;
            };
            let Some(era) = call.era.clone() else { return };
            call.announced = true;
            (call.conversation.clone(), era, call.link.as_ref().map(|l| l.room_id.clone()))
        };
        // A call link's call has no group to tell: it goes in the call
        // history instead (Desktop's `updateCallHistoryForAdhocCall`).
        if let Some(room_id) = link {
            return self.record_link_call(&room_id, &era, true).await;
        }
        self.send_group_call_update(&conversation, era).await;
    }

    async fn send_group_call_update(&self, conversation: &ConversationId, era: String) {
        let Some(context) = self.group_context(conversation).await else { return };
        let timestamp = self.next_timestamp() as u64;
        let data = DataMessage {
            group_v2: Some(context),
            group_call_update: Some(data_message::GroupCallUpdate { era_id: Some(era) }),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation: conversation.clone(), body: data.into(), timestamp, message: None });
    }

    /// Someone's `groupCallUpdate` (or ours, from another device): a call
    /// in the history if it's new to us. Desktop peeks the call first and
    /// takes who started it from the calling server; we take the sender,
    /// which is who started it unless they joined someone else's.
    pub(super) async fn on_group_call_started(&self, conversation: ConversationId, by: Option<String>, era: Option<String>, at: i64) {
        // Without an era there's no call to name (Desktop records none).
        let Some(era) = era else { return };
        let call_id = signal_calls::call_id_from_era(&era);
        let active = self.group_call.borrow().as_ref().is_some_and(|g| g.conversation == conversation && g.era.as_deref() == Some(&era));
        let record = GroupCallRecord { call_id, era: Some(era), incoming: by.is_some(), by, at };
        self.group_call_history(&conversation, record, CallHistoryEvent::Started, active, HistorySource::IfNew).await;
    }

    /// Whose device a demux id is in the call (ours included).
    fn demux_aci(&self, client_id: ClientId, demux_id: u32) -> Option<String> {
        let call = self.group_call.borrow();
        let call = call.as_ref().filter(|g| g.client_id == Some(client_id))?;
        if call.our_demux == Some(demux_id) {
            return Some(self.self_aci());
        }
        call.participants.iter().find(|p| p.demux_id == demux_id).and_then(|p| aci_of(&p.user_id))
    }

    /// Asks for the video of the participants on screen with their camera
    /// on: the one shown large at 720p, the rest at 360p (Desktop's
    /// `setGroupCallVideoRequest` asks each at its tile's size). Video
    /// not asked for isn't sent.
    fn request_group_video(&self, client_id: ClientId) {
        let sizes: Vec<_> = {
            let call = self.group_call.borrow();
            let Some(call) = call.as_ref().filter(|g| g.client_id == Some(client_id)) else { return };
            call.participants
                .iter()
                .filter(|p| !p.video_muted)
                .filter_map(|p| match &call.visible {
                    None => Some((p.demux_id, 640, 360)),
                    Some((_, Some(main))) if *main == p.demux_id => Some((p.demux_id, 1280, 720)),
                    Some((visible, _)) if visible.contains(&p.demux_id) => Some((p.demux_id, 640, 360)),
                    Some(_) => None,
                })
                .collect()
        };
        self.with_calls(|calls| calls.request_group_video(client_id, sizes));
    }

    /// The UI's tiles changed (a page, or the speaker view).
    pub(super) fn show_group_video(&self, visible: Vec<u32>, main: Option<u32>) {
        let client_id = {
            let mut call = self.group_call.borrow_mut();
            let Some(call) = call.as_mut().filter(|g| g.client_id.is_some()) else { return };
            call.visible = Some((visible, main));
            call.client_id
        };
        if let Some(client_id) = client_id {
            self.request_group_video(client_id);
        }
    }

    /// Desktop's call reactions.
    pub(super) fn call_react(&self, emoji: String) {
        let Some(client_id) = self.group_call.borrow().as_ref().filter(|g| g.joined).and_then(|g| g.client_id) else { return };
        self.with_calls(|calls| calls.group_react(client_id, emoji));
    }

    pub(super) fn raise_hand(&self, raise: bool) {
        let Some(client_id) = self.group_call.borrow().as_ref().filter(|g| g.joined).and_then(|g| g.client_id) else { return };
        self.with_calls(|calls| calls.group_raise_hand(client_id, raise));
    }

    /// Desktop's "Mute audio" on someone's tile (`remoteMute.send`).
    pub(super) fn remote_mute(&self, demux_id: u32) {
        let client_id = {
            let call = self.group_call.borrow();
            let Some(call) = call.as_ref().filter(|g| g.joined && g.our_demux != Some(demux_id)) else { return };
            call.client_id
        };
        if let Some(client_id) = client_id {
            self.with_calls(|calls| calls.group_remote_mute(client_id, demux_id));
        }
    }

    fn group_key(&self, client_id: ClientId) -> Option<[u8; 32]> {
        self.group_call.borrow().as_ref().filter(|g| g.client_id == Some(client_id) && g.link.is_none()).map(|g| g.key)
    }

    /// A call link's call, connecting through `client_id` (made by
    /// `call_links.rs`), as Desktop's `startCallLinkLobby` and join.
    pub(super) fn begin_link_call(&self, conversation: ConversationId, video: bool, link: super::call_links::LinkCallState) {
        *self.group_call.borrow_mut() =
            Some(GroupCall { video, link: Some(link), ..GroupCall::new(conversation, [0; 32], CallStatus::Connecting) });
    }

    /// Hands the link call begun for `conversation` its client. `false`:
    /// it was hung up meanwhile.
    pub(super) fn attach_link_client(&self, conversation: &ConversationId, client_id: ClientId) -> bool {
        let mut call = self.group_call.borrow_mut();
        match call.as_mut().filter(|g| g.conversation == *conversation && g.client_id.is_none() && g.link.is_some()) {
            Some(call) => {
                call.client_id = Some(client_id);
                true
            }
            None => false,
        }
    }

    /// The link call's client, for an admin's approvals.
    pub(super) fn link_client_id(&self) -> Option<ClientId> {
        self.group_call.borrow().as_ref().filter(|g| g.link.as_ref().is_some_and(|l| l.admin)).and_then(|g| g.client_id)
    }

    /// Drops a link call begun for `conversation` that never got its client.
    pub(super) fn abandon_link_call(&self, conversation: &ConversationId) {
        let mut call = self.group_call.borrow_mut();
        if call.as_ref().is_some_and(|g| g.conversation == *conversation && g.client_id.is_none() && g.link.is_some()) {
            call.take();
        }
    }

    /// The room of the link call we're in or joining, if it's that.
    pub(super) fn link_call_room(&self) -> Option<String> {
        self.group_call.borrow().as_ref().and_then(|g| g.link.as_ref()).map(|l| l.room_id.clone())
    }

    /// One of RingRTC's HTTP requests (to the calling server).
    pub(super) fn group_http(self: &Rc<Self>, request_id: u32, request: signal_calls::HttpRequest) {
        let this = self.clone();
        tokio::task::spawn_local(async move {
            let response = calling_server_request(method_of(request.method), request).await;
            this.with_calls(|calls| calls.received_http_response(request_id, response));
        });
    }

    /// A group call's opaque message to people (or the whole group).
    pub(super) async fn send_opaque(self: &Rc<Self>, recipients: Vec<Vec<u8>>, group_id: Option<Vec<u8>>, data: Vec<u8>, urgent: bool) {
        let Some(manager) = self.manager() else { return };
        let mut to: Vec<Aci> = recipients.iter().filter_map(|r| <[u8; 16]>::try_from(r.as_slice()).ok()).map(Aci::from_uuid_bytes).collect();
        if let Some(group_id) = group_id {
            let Some(key) = self.group_for_id(&manager, &group_id).await else { return };
            let me = self.self_aci();
            let members = self.group_members(key).await;
            to = members
                .into_iter()
                .filter(|m| m.service_id_string() != me)
                .filter(|m| to.is_empty() || to.contains(m))
                .collect();
        }
        let urgency = if urgent { call_message::opaque::Urgency::HandleImmediately } else { call_message::opaque::Urgency::Droppable };
        let message = CallMessage {
            opaque: Some(call_message::Opaque { data: Some(data), urgency: Some(urgency.into()) }),
            ..Default::default()
        };
        for aci in to {
            let mut manager = manager.clone();
            let message = message.clone();
            let timestamp = self.next_timestamp() as u64;
            tokio::task::spawn_local(async move {
                if let Err(error) = manager.send_message(aci, ContentBody::CallMessage(message), timestamp).await {
                    warn!(%error, "failed to send a group call message");
                }
            });
        }
    }

    pub(super) fn group_sharing(&self) -> Option<(bool, bool)> {
        self.group_call.borrow().as_ref().filter(|g| g.client_id.is_some()).map(|g| (g.sharing, g.camera_before_sharing))
    }

    pub(super) fn set_group_sharing(&self, sharing: bool, camera_before: Option<bool>) {
        if let Some(call) = self.group_call.borrow_mut().as_mut() {
            call.sharing = sharing;
            if let Some(camera) = camera_before {
                call.camera_before_sharing = camera;
            }
        }
    }
}

/// Makes one of RingRTC's requests to the calling server. `None`: it
/// failed before an answer.
pub(super) async fn calling_server_request(
    method: reqwest::Method,
    request: signal_calls::HttpRequest,
) -> Option<signal_calls::HttpResponse> {
    let client = http()?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    match builder.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            match response.bytes().await {
                Ok(body) => Some(signal_calls::HttpResponse { status: status.into(), body: body.to_vec() }),
                Err(_) => None,
            }
        }
        Err(error) => {
            warn!(error = %format!("{:#}", anyhow::Error::from(error)), "calling server request failed");
            None
        }
    }
}

/// RingRTC's HTTP method, for reqwest.
pub(super) fn method_of(method: signal_calls::HttpMethod) -> reqwest::Method {
    match method {
        signal_calls::HttpMethod::Get => reqwest::Method::GET,
        signal_calls::HttpMethod::Put => reqwest::Method::PUT,
        signal_calls::HttpMethod::Post => reqwest::Method::POST,
        signal_calls::HttpMethod::Delete => reqwest::Method::DELETE,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_calling_server_client_trusts_signals_root() {
        assert!(crate::signal_tls().is_ok());
        assert!(super::http().is_some());
    }

    /// Reaches the real calling server (run with --ignored).
    #[tokio::test]
    #[ignore]
    async fn the_calling_server_answers_over_tls() {
        let response = super::http().unwrap().get("https://sfu.voip.signal.org/v2/conference/participants").send().await;
        // Unauthorized without a membership proof, but it answered.
        assert!(response.unwrap().status().is_client_error());
    }
}
