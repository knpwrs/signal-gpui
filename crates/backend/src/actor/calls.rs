//! The actor's side of 1:1 calls: commands from the UI, RingRTC's events,
//! and `CallMessage`s from the wire. RingRTC runs the call; this keeps the
//! UI's view of it, sends its signaling, and records the call in the chat.

use presage::libsignal_service::content::ContentBody;
use presage::proto::CallMessage;
use signal_calls::{
    Answer, CallEvent, CallId, CallMediaType, EndReason, Ice, IceCandidate, Offer, ReceivedAnswer, ReceivedBusy,
    ReceivedHangup, ReceivedIce, ReceivedOffer, State,
};
use signal_store::{CallOutcome, Notice};

use super::*;
use crate::calling::{Incoming, from_call_message, to_call_message};
use crate::{CallStatus, CallUpdate};

/// The call in progress.
pub(super) struct ActiveCall {
    /// Their ACI.
    peer: String,
    call_id: Option<CallId>,
    conversation: ConversationId,
    video: bool,
    incoming: bool,
    status: CallStatus,
    remote_video: bool,
    remote_audio: bool,
    sharing: bool,
    remote_sharing: bool,
    /// Our camera was on when we started presenting (it comes back after).
    camera_before_sharing: bool,
    /// When it started, for the history notice (unix millis).
    started_at: i64,
    /// It connected at some point (for the history notice).
    connected: bool,
}

impl ActiveCall {
    fn update(&self) -> CallUpdate {
        CallUpdate {
            conversation: self.conversation.clone(),
            status: self.status.clone(),
            video: self.video,
            incoming: self.incoming,
            remote_video: self.remote_video,
            remote_audio: self.remote_audio,
            sharing: self.sharing,
            remote_sharing: self.remote_sharing,
            group: false,
            participants: Vec::new(),
            ringer: None,
        }
    }
}

/// Uses the chosen devices (where they're still there).
fn apply_choices(calls: &mut signal_calls::Calls, choices: &crate::CallDevices) {
    if let Some(id) = &choices.microphone
        && let Err(error) = calls.set_microphone(id)
    {
        warn!(error = %format!("{error:#}"), "chosen microphone unavailable");
    }
    if let Some(id) = &choices.speaker
        && let Err(error) = calls.set_speaker(id)
    {
        warn!(error = %format!("{error:#}"), "chosen speaker unavailable");
    }
    calls.set_camera(choices.camera.clone());
}

/// How a call went, from how it ended (Desktop's call history).
pub(super) fn outcome(incoming: bool, connected: bool, reason: Option<EndReason>) -> CallOutcome {
    match (incoming, connected, reason) {
        (_, true, _) => CallOutcome::Accepted,
        (true, false, Some(EndReason::AcceptedOnAnotherDevice)) => CallOutcome::Accepted,
        (true, false, Some(EndReason::LocalHangup | EndReason::DeclinedOnAnotherDevice)) => CallOutcome::Declined,
        (true, false, _) => CallOutcome::Missed,
        (false, false, _) => CallOutcome::Unanswered,
    }
}

impl Actor {
    /// RingRTC, set up on first use (it opens the audio devices).
    pub(super) fn call_session(&self) -> bool {
        if self.calls.borrow().is_some() {
            return true;
        }
        let me = self.self_aci();
        if me.is_empty() {
            return false;
        }
        let events = self.call_events.clone();
        let emit = move |event: CallEvent| {
            let _ = events.unbounded_send(event);
        };
        match signal_calls::Calls::new(emit, &me, self.video.local.clone(), self.video.remote.clone(), self.video.group.clone()) {
            Ok(mut calls) => {
                // The devices chosen in Settings (read ahead, in `load_call_choices`).
                let choices = self.call_choices.borrow().clone();
                apply_choices(&mut calls, &choices);
                *self.calls.borrow_mut() = Some(calls);
                true
            }
            Err(error) => {
                self.error("calls aren't available", format!("{error:#}"));
                false
            }
        }
    }

    pub(super) async fn load_share_sources(&self) {
        let sources = tokio::task::spawn_blocking(signal_calls::Calls::share_sources)
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| format!("{e:#}")));
        self.emit(Event::ShareSources(sources));
    }

    /// Desktop's "Present", in place of the camera.
    pub(super) fn start_sharing(&self, source: String) {
        let group = self.group_sharing().is_some();
        if !group && self.call.borrow().as_ref().is_none_or(|c| !matches!(c.status, CallStatus::Connected { .. })) {
            return;
        }
        let mut result = Ok(());
        let mut camera_on = false;
        self.with_calls(|calls| {
            camera_on = calls.camera_on();
            result = calls.start_sharing(&source);
        });
        match result {
            Ok(()) => {
                if group {
                    self.set_group_sharing(true, Some(camera_on));
                    self.group_presenting(true);
                    self.group_media(None, Some(true));
                } else if let Some(call) = self.call.borrow_mut().as_mut() {
                    call.sharing = true;
                    call.camera_before_sharing = camera_on;
                }
                self.emit_call();
            }
            Err(error) => self.error("couldn't share your screen", format!("{error:#}")),
        }
    }

    pub(super) fn stop_sharing(&self) {
        if let Some((sharing, camera)) = self.group_sharing() {
            if sharing {
                self.set_group_sharing(false, None);
                self.with_calls(|calls| calls.stop_sharing(camera));
                self.group_presenting(false);
                self.group_media(None, Some(camera));
                self.emit_call();
            }
            return;
        }
        let camera = {
            let mut call = self.call.borrow_mut();
            let Some(call) = call.as_mut().filter(|c| c.sharing) else { return };
            call.sharing = false;
            call.camera_before_sharing
        };
        self.with_calls(|calls| calls.stop_sharing(camera));
        self.emit_call();
    }

    /// The saved device choices, kept for when RingRTC starts.
    pub(super) async fn load_call_choices(&self) {
        let mut choices = crate::CallDevices::default();
        for kind in [crate::DeviceKind::Microphone, crate::DeviceKind::Speaker, crate::DeviceKind::Camera] {
            let saved = self.db.setting(kind.setting()).await.ok().flatten().filter(|id| !id.is_empty());
            match kind {
                crate::DeviceKind::Microphone => choices.microphone = saved,
                crate::DeviceKind::Speaker => choices.speaker = saved,
                crate::DeviceKind::Camera => choices.camera = saved,
            }
        }
        *self.call_choices.borrow_mut() = choices;
    }

    pub(super) async fn load_call_devices(&self) {
        self.load_call_choices().await;
        let mut current = self.call_choices.borrow().clone();
        self.with_calls(|calls| current.devices = calls.devices());
        self.emit(Event::CallDevices(current));
    }

    pub(super) async fn set_call_device(&self, kind: crate::DeviceKind, id: String) {
        if let Err(error) = self.db.set_setting(kind.setting(), &id).await {
            return self.error("couldn't save the device", error);
        }
        self.load_call_choices().await;
        let choices = self.call_choices.borrow().clone();
        self.with_calls(|calls| apply_choices(calls, &choices));
        self.load_call_devices().await;
    }

    pub(super) fn with_calls(&self, f: impl FnOnce(&mut signal_calls::Calls)) {
        if self.call_session()
            && let Some(calls) = self.calls.borrow_mut().as_mut()
        {
            f(calls);
        }
    }

    pub(super) fn emit_call(&self) {
        let update = match self.call.borrow().as_ref() {
            Some(call) => Some(call.update()),
            None => self.group_call.borrow().as_ref().map(super::group_calls::GroupCall::update),
        };
        self.emit(Event::Call(update));
    }

    pub(super) fn start_call(&self, conversation: ConversationId, video: bool) {
        if conversation.as_group().is_some() {
            return self.start_group_call(conversation, video);
        }
        if self.in_any_call() {
            return self.error("can't call", "you're already in a call");
        }
        let Some(peer) = conversation.as_direct().map(str::to_owned) else { return };
        let Some(manager) = self.manager() else { return self.call_not_started() };
        if !self.call_session() {
            return self.call_not_started();
        }
        let device = u32::from(manager.device_id());
        *self.call.borrow_mut() = Some(ActiveCall {
            peer: peer.clone(),
            call_id: None,
            conversation,
            video,
            incoming: false,
            status: CallStatus::Calling,
            remote_video: false,
            remote_audio: true,
            sharing: false,
            remote_sharing: false,
            camera_before_sharing: false,
            started_at: now_millis(),
            connected: false,
        });
        self.with_calls(|calls| {
            calls.set_video_enabled(video);
            calls.call(&peer, video, device);
        });
        self.emit_call();
    }

    pub(super) fn accept_call(&self, video: bool) {
        if self.call.borrow().is_none() && self.accept_group_call(video) {
            return;
        }
        let call_id = {
            let mut call = self.call.borrow_mut();
            let Some(call) = call.as_mut().filter(|c| c.incoming && c.status == CallStatus::Ringing) else { return };
            call.status = CallStatus::Connecting;
            call.call_id
        };
        let Some(call_id) = call_id else { return };
        self.with_calls(|calls| {
            calls.set_video_enabled(video);
            calls.accept(call_id);
        });
        self.emit_call();
    }

    /// Declines, ends, or stops calling.
    pub(super) fn hang_up(&self) {
        if self.call.borrow().is_none() {
            self.end_group_call();
            return;
        }
        self.with_calls(|calls| calls.hangup());
    }

    pub(super) fn set_call_audio(&self, enabled: bool) {
        self.with_calls(|calls| calls.set_audio_enabled(enabled));
        self.group_media(Some(enabled), None);
    }

    /// The camera, in a call or for the lobby's preview.
    pub(super) fn set_call_video(&self, enabled: bool) {
        if !enabled && self.calls.borrow().is_none() {
            return;
        }
        // While presenting, remember it for when presenting stops.
        if let Some(call) = self.call.borrow_mut().as_mut().filter(|c| c.sharing) {
            call.camera_before_sharing = enabled;
            return;
        }
        if self.group_sharing().is_some_and(|(sharing, _)| sharing) {
            return self.set_group_sharing(true, Some(enabled));
        }
        self.with_calls(|calls| calls.set_video_enabled(enabled));
        self.group_media(None, Some(enabled));
    }

    /// RingRTC's events, on the actor's thread.
    pub(super) async fn call_loop(self: Rc<Self>, mut events: mpsc::UnboundedReceiver<CallEvent>) {
        while let Some(event) = events.next().await {
            match event {
                CallEvent::Send { recipient, call_id, device, message } => {
                    let message = to_call_message(call_id, device, &message);
                    self.send_call_message(&recipient, message);
                }
                CallEvent::State { peer, call_id, state } => self.on_call_state(peer, call_id, state).await,
                CallEvent::RemoteVideo { peer, enabled } => {
                    if let Some(call) = self.call.borrow_mut().as_mut().filter(|c| c.peer == peer) {
                        call.remote_video = enabled;
                    }
                    self.emit_call();
                }
                CallEvent::RemoteSharing { peer, enabled } => {
                    if let Some(call) = self.call.borrow_mut().as_mut().filter(|c| c.peer == peer) {
                        call.remote_sharing = enabled;
                    }
                    self.emit_call();
                }
                CallEvent::RemoteAudio { peer, enabled } => {
                    if let Some(call) = self.call.borrow_mut().as_mut().filter(|c| c.peer == peer) {
                        call.remote_audio = enabled;
                    }
                    self.emit_call();
                }
                CallEvent::SendOpaque { recipient, data, urgent } => {
                    self.send_opaque(vec![recipient], None, data, urgent).await;
                }
                CallEvent::SendOpaqueToGroup { group_id, data, urgent, recipients } => {
                    self.send_opaque(recipients, Some(group_id), data, urgent).await;
                }
                CallEvent::Http { request_id, request } => self.group_http(request_id, request),
                CallEvent::Group(event) => self.on_group_event(event).await,
            }
        }
    }

    fn send_call_message(self: &Rc<Self>, recipient: &str, message: CallMessage) {
        let Some(aci) = Aci::parse_from_service_id_string(recipient) else { return };
        let _ = self.call_sends.unbounded_send((aci, message));
    }

    /// Sends call messages one at a time, in order.
    pub(super) async fn call_send_loop(self: Rc<Self>, mut sends: mpsc::UnboundedReceiver<(Aci, CallMessage)>) {
        while let Some((aci, message)) = sends.next().await {
            let Some(mut manager) = self.manager() else { continue };
            let timestamp = self.next_timestamp() as u64;
            if let Err(error) = manager.send_message(aci, ContentBody::CallMessage(message), timestamp).await {
                warn!(%error, "failed to send a call message");
            }
        }
    }

    /// A call didn't start: the lobby's camera preview goes off with it.
    pub(super) fn call_not_started(&self) {
        if self.calls.borrow().is_some() {
            self.with_calls(|calls| calls.set_video_enabled(false));
        }
        self.emit_call();
    }

    async fn on_call_state(self: &Rc<Self>, peer: String, call_id: CallId, state: State) {
        info!(?state, "call state");
        match state {
            State::Outgoing { .. } => {
                if let Some(call) = self.call.borrow_mut().as_mut().filter(|c| c.peer == peer) {
                    call.call_id = Some(call_id);
                }
                self.proceed(call_id, false).await;
            }
            State::Incoming { video } => {
                // One call at a time.
                if self.call.borrow().is_some() || self.group_call.borrow().is_some() {
                    self.with_calls(|calls| calls.drop_call(call_id));
                    return;
                }
                let conversation = ConversationId::direct(&peer);
                *self.call.borrow_mut() = Some(ActiveCall {
                    peer: peer.clone(),
                    call_id: Some(call_id),
                    conversation: conversation.clone(),
                    video,
                    incoming: true,
                    status: CallStatus::Ringing,
                    remote_video: video,
                    remote_audio: true,
                    sharing: false,
                    remote_sharing: false,
                    camera_before_sharing: false,
                    started_at: now_millis(),
                    connected: false,
                });
                // As Desktop: relay everything for someone without a chat (not
                // a contact), so they don't learn our IP address.
                let stranger = self.db.conversation(&conversation).await.ok().flatten().is_none();
                self.proceed(call_id, stranger).await;
                self.emit_call();
            }
            State::Ringing => {
                if let Some(call) = self.call.borrow_mut().as_mut().filter(|c| !c.incoming) {
                    call.status = CallStatus::RingingThem;
                }
                self.emit_call();
            }
            State::Connecting => {
                if let Some(call) = self.call.borrow_mut().as_mut() {
                    call.status = CallStatus::Connecting;
                }
                self.emit_call();
            }
            State::Connected => {
                if let Some(call) = self.call.borrow_mut().as_mut() {
                    // (Again after reconnecting: the clock keeps running.)
                    if !matches!(call.status, CallStatus::Connected { .. }) {
                        call.status = CallStatus::Connected { since: now_millis() };
                    }
                    call.connected = true;
                }
                self.emit_call();
            }
            State::Ended(reason) => self.end_call(&peer, call_id, Some(reason)).await,
            State::Rejected | State::Concluded => self.end_call(&peer, call_id, None).await,
        }
    }

    /// Fetches Signal's relays and lets RingRTC go ahead.
    async fn proceed(self: &Rc<Self>, call_id: CallId, hide_ip: bool) {
        let relays = match self.manager() {
            Some(manager) => crate::calling::relays(&manager).await.unwrap_or_else(|error| {
                warn!(error = %format!("{error:#}"), "couldn't get call relays");
                Vec::new()
            }),
            None => Vec::new(),
        };
        self.with_calls(|calls| calls.proceed(call_id, &relays, hide_ip));
    }

    /// The call is over: note it in the chat, and clear it.
    async fn end_call(self: &Rc<Self>, peer: &str, call_id: CallId, reason: Option<EndReason>) {
        let ended = {
            let mut call = self.call.borrow_mut();
            match call.as_ref() {
                // (An outgoing call has no id until RingRTC assigns one.)
                Some(c) if c.peer == peer && c.call_id.is_none_or(|id| id == call_id) => call.take(),
                _ => None,
            }
        };
        let Some(call) = ended else { return };
        self.with_calls(|calls| calls.reset_media());
        let outcome = outcome(call.incoming, call.connected, reason);
        let notice = Notice::Call {
            incoming: call.incoming,
            video: call.video,
            outcome,
            call_id: call.call_id.map(|id| id.as_u64() as i64),
        };
        match self.db.record_call(&call.conversation, call.started_at, &notice).await {
            Ok(Some(id)) => self.emit(Event::MessageChanged { conversation: call.conversation.clone(), message_id: id }),
            Ok(None) => {}
            Err(error) => self.error("failed to record the call", error),
        }
        // Our other devices' call history (all of them saw a missed call
        // ring, so that one needs no word).
        if let (Some(call_id), false) = (call.call_id, outcome == CallOutcome::Missed) {
            self.sync_call_event(&call.peer, call_id, call.started_at, call.video, call.incoming, outcome);
        }
        self.emit(Event::ConversationsChanged);
        self.emit_call();
    }

    /// Desktop's call event sync: how a 1:1 call went, for our other devices'
    /// call history.
    fn sync_call_event(&self, peer: &str, call_id: CallId, at: i64, video: bool, incoming: bool, outcome: CallOutcome) {
        use presage::proto::sync_message::call_event::{Direction, Event as CallEventKind, Type};
        let Some(aci) = Aci::parse_from_service_id_string(peer) else { return };
        let event = presage::proto::sync_message::CallEvent {
            conversation_id: Some(aci.service_id_binary()),
            call_id: Some(call_id.as_u64()),
            timestamp: Some(at as u64),
            r#type: Some(if video { Type::VideoCall } else { Type::AudioCall }.into()),
            direction: Some(if incoming { Direction::Incoming } else { Direction::Outgoing }.into()),
            event: Some(if outcome == CallOutcome::Accepted { CallEventKind::Accepted } else { CallEventKind::NotAccepted }.into()),
        };
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::CallEvent(event)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// A call event from another of our devices: its record of a call,
    /// added to the chat or merged with ours.
    pub(super) async fn on_call_event(&self, event: crate::mapping::CallEventSync) {
        let conversation = ConversationId::direct(&event.peer);
        let result = match event.outcome {
            None => self.db.remove_call(&conversation, event.call_id).await.map(|id| id.into_iter().collect::<Vec<_>>()),
            Some(outcome) => {
                let notice = Notice::Call { incoming: event.incoming, video: event.video, outcome, call_id: Some(event.call_id) };
                self.db.record_call(&conversation, event.timestamp, &notice).await.map(|id| id.into_iter().collect())
            }
        };
        match result {
            Ok(ids) => {
                for id in ids {
                    self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                }
                self.emit(Event::ConversationsChanged);
            }
            Err(error) => self.error("failed to record a call", error),
        }
    }

    /// Their identity key and ours, without the type byte (as Desktop passes
    /// them to RingRTC).
    async fn call_keys(&self, peer: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        let store = presage::store::Store::aci_protocol_store(&self.store);
        let ours = store.get_identity_key_pair().await.ok()?.identity_key().serialize();
        let address = ProtocolAddress::new(peer.to_owned(), DeviceId::new(1).ok()?);
        let theirs = store.get_identity(&address).await.ok()??.serialize();
        Some((theirs.get(1..)?.to_vec(), ours.get(1..)?.to_vec()))
    }

    /// A `CallMessage` from `sender`'s device.
    pub(super) async fn on_call_message(
        self: &Rc<Self>,
        sender: &ServiceId,
        sender_device: u32,
        server_timestamp: i64,
        message: &CallMessage,
    ) {
        let Some(manager) = self.manager() else { return };
        let ServiceId::Aci(aci) = sender else { return };
        let peer = aci.service_id_string();
        let incoming = from_call_message(message);
        if incoming.is_empty() || !self.call_session() {
            return;
        }
        let keys = self.call_keys(&peer).await;
        let our_device = u32::from(manager.device_id());
        for message in incoming {
            match message {
                Incoming::Offer { call_id, video, opaque } => {
                    let Some((theirs, ours)) = keys.clone() else { continue };
                    let media = if video { CallMediaType::Video } else { CallMediaType::Audio };
                    let Ok(offer) = Offer::new(media, opaque) else { continue };
                    let age = signal_calls::offer_age(server_timestamp.max(0) as u64, now_millis().max(0) as u64);
                    let received = ReceivedOffer {
                        offer,
                        age,
                        sender_device_id: sender_device,
                        receiver_device_id: our_device,
                        sender_identity_key: theirs,
                        receiver_identity_key: ours,
                    };
                    self.with_calls(|calls| calls.received_offer(&peer, call_id, received));
                }
                Incoming::Answer { call_id, opaque } => {
                    let Some((theirs, ours)) = keys.clone() else { continue };
                    let Ok(answer) = Answer::new(opaque) else { continue };
                    let received = ReceivedAnswer {
                        answer,
                        sender_device_id: sender_device,
                        sender_identity_key: theirs,
                        receiver_identity_key: ours,
                    };
                    self.with_calls(|calls| calls.received_answer(&peer, call_id, received));
                }
                Incoming::Ice { call_id, candidates } => {
                    let ice = Ice { candidates: candidates.into_iter().map(IceCandidate::new).collect() };
                    let received = ReceivedIce { ice, sender_device_id: sender_device };
                    self.with_calls(|calls| calls.received_ice(&peer, call_id, received));
                }
                Incoming::Hangup { call_id, hangup } => {
                    let received = ReceivedHangup { hangup, sender_device_id: sender_device };
                    self.with_calls(|calls| calls.received_hangup(&peer, call_id, received));
                }
                Incoming::Busy { call_id } => {
                    let received = ReceivedBusy { sender_device_id: sender_device };
                    self.with_calls(|calls| calls.received_busy(&peer, call_id, received));
                }
                Incoming::Opaque { data } => {
                    let sender = aci.service_id_binary().get(..16).map(<[u8]>::to_vec).unwrap_or_default();
                    let age = signal_calls::offer_age(server_timestamp.max(0) as u64, now_millis().max(0) as u64);
                    self.with_calls(|calls| calls.received_call_message(sender, sender_device, our_device, data, age));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_follow_desktops_call_history() {
        use EndReason::*;
        assert_eq!(outcome(true, true, Some(RemoteHangup)), CallOutcome::Accepted);
        assert_eq!(outcome(true, false, Some(AcceptedOnAnotherDevice)), CallOutcome::Accepted);
        assert_eq!(outcome(true, false, Some(LocalHangup)), CallOutcome::Declined);
        assert_eq!(outcome(true, false, Some(DeclinedOnAnotherDevice)), CallOutcome::Declined);
        assert_eq!(outcome(true, false, Some(RemoteHangup)), CallOutcome::Missed);
        assert_eq!(outcome(true, false, None), CallOutcome::Missed);
        assert_eq!(outcome(false, false, Some(Timeout)), CallOutcome::Unanswered);
        assert_eq!(outcome(false, true, Some(LocalHangup)), CallOutcome::Accepted);
    }
}
