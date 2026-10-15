//! Group calls, as Desktop makes them: RingRTC's group call client through
//! Signal's calling server. We hand it a membership proof and the members
//! when it asks, carry its HTTP requests and opaque call messages, ring the
//! group when we start a call no one's in, and tell the group ("… started a
//! group call").

use std::sync::OnceLock;

use presage::libsignal_service::content::ContentBody;
use presage::proto::{CallMessage, call_message};
use signal_calls::{ClientId, GroupEvent, Participant, RingCancelReason};
use signal_store::Notice;

use super::*;
use crate::{CallParticipant, CallStatus, CallUpdate};

/// Desktop rings groups of up to this many members (`maxGroupCallRingSize`);
/// bigger ones are only told.
const MAX_RING_SIZE: usize = 16;

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
    rang: bool,
    announced: bool,
    joined: bool,
    sharing: bool,
    camera_before_sharing: bool,
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
fn http() -> Option<&'static reqwest::Client> {
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
        *self.group_call.borrow_mut() = Some(GroupCall {
            conversation,
            key,
            client_id: Some(client_id),
            status: CallStatus::Connecting,
            video,
            participants: Vec::new(),
            ring: None,
            starter: true,
            era: None,
            rang: false,
            announced: false,
            joined: false,
            sharing: false,
            camera_before_sharing: false,
        });
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
    pub(super) fn end_group_call(&self) -> bool {
        let Some(call) = self.group_call.borrow_mut().take() else { return false };
        match (call.client_id, call.ring) {
            (Some(client_id), _) => self.with_calls(|calls| {
                calls.group_end(client_id);
                calls.reset_media();
            }),
            (None, Some((group_id, ring_id, _))) => {
                self.with_calls(|calls| calls.cancel_group_ring(group_id, ring_id, RingCancelReason::DeclinedByUser));
            }
            (None, None) => {}
        }
        self.emit_call();
        true
    }

    /// Our microphone and camera in a group call (as well as the tracks).
    pub(super) fn group_media(&self, audio: Option<bool>, video: Option<bool>) {
        let Some(client_id) = self.group_call.borrow().as_ref().and_then(|g| g.client_id) else { return };
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
            GroupEvent::Joined { client_id, joined } => {
                let ring = {
                    let mut call = self.group_call.borrow_mut();
                    let Some(call) = call.as_mut().filter(|g| g.client_id == Some(client_id)) else { return };
                    call.joined = joined.is_some();
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
                self.emit_call();
            }
            GroupEvent::Participants { client_id, participants } => {
                let sizes: Vec<_> = participants.iter().filter(|p| !p.video_muted).map(|p| (p.demux_id, 640, 360)).collect();
                if let Some(call) = self.group_call.borrow_mut().as_mut().filter(|g| g.client_id == Some(client_id)) {
                    call.participants = participants;
                }
                self.with_calls(|calls| calls.request_group_video(client_id, sizes));
                self.emit_call();
            }
            GroupEvent::Peek { client_id, peek } => {
                if let Some(call) = self.group_call.borrow_mut().as_mut().filter(|g| g.client_id == Some(client_id)) {
                    call.era = peek.era_id;
                }
                self.announce_group_call(client_id).await;
            }
            GroupEvent::PeekResult { request_id, peek } => {
                let Some(conversation) = self.peeks.borrow_mut().remove(&request_id) else { return };
                let users = peek.map(|p| p.users.iter().filter_map(|u| aci_of(u)).collect()).unwrap_or_default();
                self.emit(Event::GroupPeek { conversation, users });
            }
            GroupEvent::Ended { client_id } => {
                let ended = {
                    let mut call = self.group_call.borrow_mut();
                    if call.as_ref().is_some_and(|g| g.client_id == Some(client_id)) { call.take() } else { None }
                };
                if ended.is_some() {
                    self.with_calls(|calls| {
                        calls.group_end(client_id);
                        calls.reset_media();
                    });
                    self.emit_call();
                }
            }
            GroupEvent::Ring { group_id, ring_id, sender, ring } => self.on_group_ring(group_id, ring_id, sender, ring).await,
        }
    }

    async fn on_group_ring(&self, group_id: Vec<u8>, ring_id: i64, sender: Vec<u8>, ring: signal_calls::Ring) {
        let Some(manager) = self.manager() else { return };
        let Some(key) = self.group_for_id(&manager, &group_id).await else { return };
        match ring {
            signal_calls::Ring::Requested => {
                if self.in_any_call() || self.group_call.borrow().is_some() {
                    self.with_calls(|calls| calls.cancel_group_ring(group_id, ring_id, RingCancelReason::Busy));
                    return;
                }
                let conversation = ConversationId::group(&key);
                if self.is_blocked(&conversation).await {
                    return;
                }
                *self.group_call.borrow_mut() = Some(GroupCall {
                    conversation,
                    key,
                    client_id: None,
                    status: CallStatus::Ringing,
                    video: true,
                    participants: Vec::new(),
                    ring: Some((group_id, ring_id, aci_of(&sender).unwrap_or_default())),
                    starter: false,
                    era: None,
                    rang: false,
                    announced: false,
                    joined: false,
                    sharing: false,
                    camera_before_sharing: false,
                });
            }
            signal_calls::Ring::Over => {
                let mut call = self.group_call.borrow_mut();
                if call.as_ref().is_some_and(|g| g.client_id.is_none() && g.ring.as_ref().is_some_and(|r| r.1 == ring_id)) {
                    call.take();
                }
            }
        }
        self.emit_call();
    }

    /// Desktop's `groupCallUpdate`, sent on joining (once we know the
    /// call's era), so the group sees "… started a group call" (once per
    /// era) and looks for the call.
    async fn announce_group_call(&self, client_id: ClientId) {
        let (conversation, era, starter) = {
            let mut call = self.group_call.borrow_mut();
            let Some(call) = call.as_mut().filter(|g| g.client_id == Some(client_id) && g.joined && !g.announced) else {
                return;
            };
            let Some(era) = call.era.clone() else { return };
            call.announced = true;
            (call.conversation.clone(), era, call.starter)
        };
        let Some(context) = self.group_context(&conversation).await else { return };
        let timestamp = self.next_timestamp() as u64;
        let data = DataMessage {
            group_v2: Some(context),
            group_call_update: Some(data_message::GroupCallUpdate { era_id: Some(era.clone()) }),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation: conversation.clone(), body: data.into(), timestamp, message: None });
        // Joining someone's call isn't starting one; theirs said so already.
        if starter {
            self.on_group_call_started(conversation, None, Some(era), timestamp as i64).await;
        }
    }

    pub(super) async fn on_group_call_started(&self, conversation: ConversationId, by: Option<String>, era: Option<String>, at: i64) {
        match self.db.record_group_call(&conversation, at, &Notice::GroupCall { by, era }).await {
            Ok(Some(id)) => {
                self.emit(Event::MessageChanged { conversation, message_id: id });
                self.emit(Event::ConversationsChanged);
            }
            Ok(None) => {}
            Err(error) => self.error("failed to add notice", error),
        }
    }

    fn group_key(&self, client_id: ClientId) -> Option<[u8; 32]> {
        self.group_call.borrow().as_ref().filter(|g| g.client_id == Some(client_id)).map(|g| g.key)
    }

    /// One of RingRTC's HTTP requests (to the calling server).
    pub(super) fn group_http(self: &Rc<Self>, request_id: u32, request: signal_calls::HttpRequest) {
        let this = self.clone();
        tokio::task::spawn_local(async move {
            let method = match request.method {
                signal_calls::HttpMethod::Get => reqwest::Method::GET,
                signal_calls::HttpMethod::Put => reqwest::Method::PUT,
                signal_calls::HttpMethod::Post => reqwest::Method::POST,
                signal_calls::HttpMethod::Delete => reqwest::Method::DELETE,
            };
            let Some(client) = http() else {
                return this.with_calls(|calls| calls.received_http_response(request_id, None));
            };
            let mut builder = client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = match builder.send().await {
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
            };
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
