//! 1:1 voice and video calls through RingRTC, Signal's calling library, as
//! Signal Desktop makes them (Desktop drives the same native platform
//! through Node bindings; see RingRTC's `electron.rs`).
//!
//! [`Calls`] owns RingRTC's call manager and media. Everything RingRTC
//! reports (signaling to send, state changes) arrives as a [`CallEvent`]
//! through the callback given to [`Calls::new`]; the app sends signaling as
//! Signal `CallMessage`s and hands received ones back. Video is exchanged
//! through [`FrameSlot`]s: the latest remote frame, and the camera's for the
//! self-view.

mod camera;
mod group;
mod screen;

pub use group::{GroupEvent, GroupFrames, Participant, Peek, Ring, SFU_URL, call_id_from_era};
pub use ringrtc::core::group_call::{ClientId, RingCancelReason};
pub use ringrtc::lite::http::{Method as HttpMethod, Request as HttpRequest, Response as HttpResponse};
pub use ringrtc::lite::sfu::{DemuxId, GroupMember};

pub use screen::ShareSource;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use ringrtc::common::{CallConfig, CallEndReason};
use ringrtc::core::call_manager::CallManager;
use ringrtc::core::{group_call, signaling};
use ringrtc::lite::{http, sfu::UserId};
use ringrtc::native::{
    CallState, CallStateHandler, GroupUpdate, GroupUpdateHandler, NativeCallContext, NativePlatform, SignalingSender,
};
use ringrtc::webrtc::media::{AudioTrack, VideoFrame, VideoSink, VideoSource, VideoTrack};
use ringrtc::webrtc::peer_connection::AudioLevel;
use ringrtc::webrtc::peer_connection_factory::{self as pcf, IceServer, PeerConnectionFactory};
use ringrtc::webrtc::peer_connection_observer::NetworkRoute;

pub use ringrtc::common::{CallId, CallMediaType, DeviceId};
pub use ringrtc::core::signaling::{
    Answer, Hangup, HangupType, Ice, IceCandidate, Message, Offer, ReceivedAnswer, ReceivedBusy, ReceivedHangup,
    ReceivedIce, ReceivedOffer,
};

/// What RingRTC asks of the app, or tells it.
#[derive(Clone)]
pub enum CallEvent {
    /// Send this to `recipient` (their ACI) as a `CallMessage`; to one of
    /// their devices, or all of them.
    Send { recipient: String, call_id: CallId, device: Option<DeviceId>, message: Message },
    /// The call with `peer` changed state.
    State { peer: String, call_id: CallId, state: State },
    /// They turned their camera on or off.
    RemoteVideo { peer: String, enabled: bool },
    /// They muted or unmuted.
    RemoteAudio { peer: String, enabled: bool },
    /// They started or stopped presenting their screen.
    RemoteSharing { peer: String, enabled: bool },
    /// Our camera stopped on its own (it couldn't start, or went away).
    CameraFailed { error: String },
    /// A group call's message (ring, media keys…) for one person, as an
    /// opaque `CallMessage`.
    SendOpaque { recipient: Vec<u8>, data: Vec<u8>, urgent: bool },
    /// The same, for everyone in a group (or only `recipients`).
    SendOpaqueToGroup { group_id: Vec<u8>, data: Vec<u8>, urgent: bool, recipients: Vec<Vec<u8>> },
    /// Make this HTTP request (to the calling server) and hand the answer
    /// to [`Calls::received_http_response`].
    Http { request_id: u32, request: HttpRequest },
    Group(GroupEvent),
}

/// A call's state, as the UI shows it (RingRTC's `CallState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// They're calling. Accept with [`Calls::accept`].
    Incoming { video: bool },
    /// We're calling, before it rings.
    Outgoing { video: bool },
    Ringing,
    Connecting,
    Connected,
    Ended(EndReason),
    /// RingRTC turned an offer down (it expired, or we were busy).
    Rejected,
    /// Nothing more will happen; the call is gone.
    Concluded,
}

/// Why a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// We hung up (or declined).
    LocalHangup,
    /// They hung up (or declined).
    RemoteHangup,
    /// Another of our devices answered.
    AcceptedOnAnotherDevice,
    /// Another of our devices declined.
    DeclinedOnAnotherDevice,
    /// Another of our devices was busy, or they were.
    Busy,
    /// They need to accept a message request first.
    NeedPermission,
    /// No answer in time.
    Timeout,
    /// The connection failed or dropped.
    Failed,
}

impl From<&CallEndReason> for EndReason {
    fn from(reason: &CallEndReason) -> Self {
        match reason {
            CallEndReason::LocalHangup | CallEndReason::AppDroppedCall => Self::LocalHangup,
            CallEndReason::RemoteHangup | CallEndReason::RemoteGlare | CallEndReason::RemoteReCall => Self::RemoteHangup,
            CallEndReason::RemoteHangupAccepted => Self::AcceptedOnAnotherDevice,
            CallEndReason::RemoteHangupDeclined => Self::DeclinedOnAnotherDevice,
            CallEndReason::RemoteHangupBusy | CallEndReason::RemoteBusy => Self::Busy,
            CallEndReason::RemoteHangupNeedPermission => Self::NeedPermission,
            CallEndReason::Timeout => Self::Timeout,
            _ => Self::Failed,
        }
    }
}

/// A microphone, speaker or camera to choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// What identifies it across runs (RingRTC's unique id; a camera's name).
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Devices {
    pub microphones: Vec<Device>,
    pub speakers: Vec<Device>,
    pub cameras: Vec<Device>,
}

/// A TURN relay from Signal (`GET /v2/calling/relays`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    pub username: String,
    pub password: String,
    pub hostname: String,
    pub urls: Vec<String>,
}

/// A video frame to show: BGRA (GPUI's order), top row first.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Arc<Vec<u8>>,
    /// Increases with every frame, to tell a new one from the last shown.
    pub seq: u64,
}

/// The latest frame of one video stream, shared between the thread that
/// produces frames and the UI that shows them.
#[derive(Clone, Default)]
pub struct FrameSlot {
    frame: Arc<Mutex<Option<Frame>>>,
    seq: Arc<AtomicU64>,
}

impl FrameSlot {
    pub fn latest(&self) -> Option<Frame> {
        self.frame.lock().ok()?.clone()
    }

    pub fn clear(&self) {
        if let Ok(mut frame) = self.frame.lock() {
            *frame = None;
        }
    }

    /// Stores an RGBA frame as BGRA, mirrored if asked (the self-view is,
    /// as in every calling app).
    fn put_rgba(&self, width: u32, height: u32, rgba: &[u8], mirror: bool) {
        let bgra = rgba_to_bgra(width as usize, height as usize, rgba, mirror);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut frame) = self.frame.lock() {
            *frame = Some(Frame { width, height, bgra: Arc::new(bgra), seq });
        }
    }
}

/// RGBA to BGRA, optionally flipped left to right.
fn rgba_to_bgra(width: usize, height: usize, rgba: &[u8], mirror: bool) -> Vec<u8> {
    let mut out = vec![0; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let from = (y * width + x) * 4;
            let to_x = if mirror { width - 1 - x } else { x };
            let to = (y * width + to_x) * 4;
            if let (Some(src), Some(dst)) = (rgba.get(from..from + 4), out.get_mut(to..to + 4)) {
                dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
            }
        }
    }
    out
}

/// Desktop's camera limits (`MAX_VIDEO_WIDTH`, `_HEIGHT` and `_FPS`).
const CAMERA_LIMITS: (u16, u16, u8) = (960, 720, 30);

type Emit = Arc<dyn Fn(CallEvent) + Send + Sync>;

/// RingRTC's callbacks (on its threads), forwarded as [`CallEvent`]s.
#[derive(Clone)]
struct Handler {
    emit: Emit,
}

impl SignalingSender for Handler {
    fn send_signaling(
        &self,
        recipient_id: &str,
        call_id: CallId,
        receiver_device_id: Option<DeviceId>,
        message: signaling::Message,
    ) -> ringrtc::common::Result<()> {
        (self.emit)(CallEvent::Send { recipient: recipient_id.to_owned(), call_id, device: receiver_device_id, message });
        Ok(())
    }

    fn send_call_message(
        &self,
        recipient: UserId,
        data: Vec<u8>,
        urgency: group_call::SignalingMessageUrgency,
    ) -> ringrtc::common::Result<()> {
        let urgent = matches!(urgency, group_call::SignalingMessageUrgency::HandleImmediately);
        (self.emit)(CallEvent::SendOpaque { recipient, data, urgent });
        Ok(())
    }

    fn send_call_message_to_group(
        &self,
        group_id: group_call::GroupId,
        data: Vec<u8>,
        urgency: group_call::SignalingMessageUrgency,
        recipients: HashSet<UserId>,
    ) -> ringrtc::common::Result<()> {
        let urgent = matches!(urgency, group_call::SignalingMessageUrgency::HandleImmediately);
        let recipients = recipients.into_iter().collect();
        (self.emit)(CallEvent::SendOpaqueToGroup { group_id, data, urgent, recipients });
        Ok(())
    }

    fn send_call_message_to_adhoc_group(
        &self,
        _: Vec<u8>,
        _: group_call::SignalingMessageUrgency,
        _: u64,
        _: HashMap<UserId, Vec<u8>>,
    ) -> ringrtc::common::Result<()> {
        Ok(())
    }
}

impl CallStateHandler for Handler {
    fn handle_call_state(&self, remote_peer_id: &str, call_id: CallId, state: CallState) -> ringrtc::common::Result<()> {
        let state = match state {
            CallState::Incoming(media) => State::Incoming { video: media == CallMediaType::Video },
            CallState::Outgoing(media) => State::Outgoing { video: media == CallMediaType::Video },
            CallState::Ringing => State::Ringing,
            CallState::Connecting => State::Connecting,
            CallState::Connected => State::Connected,
            CallState::Ended(reason, _) => State::Ended(EndReason::from(&reason)),
            CallState::Rejected(_) => State::Rejected,
            CallState::Concluded => State::Concluded,
        };
        (self.emit)(CallEvent::State { peer: remote_peer_id.to_owned(), call_id, state });
        Ok(())
    }

    fn handle_remote_audio_state(&self, remote_peer_id: &str, enabled: bool) -> ringrtc::common::Result<()> {
        (self.emit)(CallEvent::RemoteAudio { peer: remote_peer_id.to_owned(), enabled });
        Ok(())
    }

    fn handle_remote_video_state(&self, remote_peer_id: &str, enabled: bool) -> ringrtc::common::Result<()> {
        (self.emit)(CallEvent::RemoteVideo { peer: remote_peer_id.to_owned(), enabled });
        Ok(())
    }

    fn handle_remote_sharing_screen(&self, remote_peer_id: &str, enabled: bool) -> ringrtc::common::Result<()> {
        (self.emit)(CallEvent::RemoteSharing { peer: remote_peer_id.to_owned(), enabled });
        Ok(())
    }

    fn handle_network_route(&self, _: &str, _: NetworkRoute) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_audio_levels(&self, _: &str, _: AudioLevel, _: AudioLevel) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_low_bandwidth_for_video(&self, _: &str, _: bool) -> ringrtc::common::Result<()> {
        Ok(())
    }
}

impl GroupUpdateHandler for Handler {
    fn handle_group_update(&self, update: GroupUpdate) -> ringrtc::common::Result<()> {
        let event = match update {
            GroupUpdate::RequestMembershipProof(client_id) => GroupEvent::NeedsProof(client_id),
            GroupUpdate::RequestGroupMembers(client_id) => GroupEvent::NeedsMembers(client_id),
            GroupUpdate::ConnectionStateChanged(client_id, state) => {
                let (connected, connecting) = group::connection(&state);
                GroupEvent::Connection { client_id, connected, connecting }
            }
            GroupUpdate::JoinStateChanged(client_id, state) => GroupEvent::Joined { client_id, joined: group::joined(&state) },
            GroupUpdate::RemoteDeviceStatesChanged(client_id, states) => {
                GroupEvent::Participants { client_id, participants: states.iter().map(Participant::from).collect() }
            }
            GroupUpdate::PeekChanged { client_id, peek_info } => GroupEvent::Peek { client_id, peek: Peek::from(&peek_info) },
            GroupUpdate::PeekResult { request_id, peek_result } => {
                GroupEvent::PeekResult { request_id, peek: peek_result.as_ref().ok().map(Peek::from) }
            }
            GroupUpdate::Ended(client_id, _, _) => GroupEvent::Ended { client_id },
            GroupUpdate::Ring { group_id, ring_id, sender_id, update } => {
                GroupEvent::Ring { group_id, ring_id: ring_id.into(), sender: sender_id, ring: update.into() }
            }
            GroupUpdate::Reactions(client_id, reactions) => {
                GroupEvent::Reactions { client_id, reactions: reactions.into_iter().map(|r| (r.demux_id, r.value)).collect() }
            }
            GroupUpdate::RaisedHands(client_id, hands) => GroupEvent::RaisedHands { client_id, hands },
            GroupUpdate::RemoteMute { client_id, mute_source } => GroupEvent::MutedBy { client_id, source: mute_source },
            GroupUpdate::ObservedRemoteMute { client_id, mute_source, mute_target } => {
                GroupEvent::ObservedMute { client_id, source: mute_source, target: mute_target }
            }
            _ => return Ok(()),
        };
        (self.emit)(CallEvent::Group(event));
        Ok(())
    }
}

impl http::Delegate for Handler {
    // Group calls talk to the calling server through the app.
    fn send_request(&self, request_id: u32, request: http::Request) {
        (self.emit)(CallEvent::Http { request_id, request });
    }
}

/// Remote video into a [`FrameSlot`].
#[derive(Clone)]
struct RemoteSink(FrameSlot);

impl VideoSink for RemoteSink {
    fn on_video_frame(&self, _: ringrtc::lite::sfu::DemuxId, frame: VideoFrame) {
        // The frame's buffer is the decoder's: copy it out and let it go.
        let (width, height) = (frame.width(), frame.height());
        let mut rgba = vec![0; width as usize * height as usize * 4];
        if frame.to_rgba(&mut rgba) {
            self.0.put_rgba(width, height, &rgba, false);
        }
    }

    fn box_clone(&self) -> Box<dyn VideoSink> {
        Box::new(self.clone())
    }
}

/// RingRTC's call manager and our media, for one call at a time.
pub struct Calls {
    manager: CallManager<NativePlatform>,
    factory: PeerConnectionFactory,
    /// The chosen camera's name (`None`: the first).
    camera_choice: Option<String>,
    audio: AudioTrack,
    /// Our microphone is on (as we last told them).
    audio_on: bool,
    video: VideoTrack,
    source: VideoSource,
    remote: FrameSlot,
    local: FrameSlot,
    camera: Option<camera::Camera>,
    /// Sharing the screen, in place of the camera.
    share: Option<screen::Share>,
    group_frames: GroupFrames,
    /// RingRTC's events' way out, for ours too.
    emit: Emit,
}

impl Calls {
    /// Opens the default microphone and speakers (through RingRTC's audio
    /// device module) and sets up the call manager. `emit` is called from
    /// RingRTC's threads.
    pub fn new(
        emit: impl Fn(CallEvent) + Send + Sync + 'static,
        self_aci: &str,
        local: FrameSlot,
        remote: FrameSlot,
        group_frames: GroupFrames,
    ) -> anyhow::Result<Self> {
        let emit: Emit = Arc::new(emit);
        let handler = Handler { emit: emit.clone() };
        let mut factory = PeerConnectionFactory::new(&pcf::AudioConfig::default(), false, "", None)
            .map_err(|e| anyhow!("{e}"))
            .context("starting WebRTC")?;
        // The system's default devices (index 0 is the default).
        let _ = factory.set_audio_playout_device(0);
        let _ = factory.set_audio_recording_device(0);
        let audio = factory.create_outgoing_audio_track().map_err(|e| anyhow!("{e}"))?;
        let source = factory.create_outgoing_video_source().map_err(|e| anyhow!("{e}"))?;
        let video = factory.create_outgoing_video_track(&source).map_err(|e| anyhow!("{e}"))?;
        source.adapt_output_format(CAMERA_LIMITS.0, CAMERA_LIMITS.1, CAMERA_LIMITS.2);
        let platform = NativePlatform::new(
            factory.clone(),
            Box::new(handler.clone()),
            // Sends go through the app's queue; failures end the call by timeout.
            true,
            Box::new(handler.clone()),
            Box::new(handler.clone()),
        );
        let mut manager =
            CallManager::new(platform, http::DelegatingClient::new(handler)).map_err(|e| anyhow!("{e}"))?;
        manager.set_self_uuid(uuid_bytes(self_aci)?).map_err(|e| anyhow!("{e}"))?;
        Ok(Self {
            manager,
            factory,
            camera_choice: None,
            audio,
            video,
            source,
            remote,
            local,
            group_frames,
            emit,
            audio_on: true,
            camera: None,
            share: None,
        })
    }

    /// Calls `peer` (their ACI). RingRTC answers with `State::Outgoing`;
    /// then [`Calls::proceed`].
    /// (Failures arrive as an `Ended` state, as for everything below.)
    pub fn call(&mut self, peer: &str, video: bool, local_device: DeviceId) {
        let media = if video { CallMediaType::Video } else { CallMediaType::Audio };
        self.manager.call(peer.to_owned(), media, local_device);
    }

    /// Goes ahead with an incoming or outgoing call, through `relays`.
    /// `hide_ip` sends everything through the relays.
    pub fn proceed(&mut self, call_id: CallId, relays: &[Relay], hide_ip: bool) {
        let ice: Vec<IceServer> = relays
            .iter()
            .map(|r| IceServer::new(r.username.clone(), r.password.clone(), r.hostname.clone(), r.urls.clone()))
            .collect();
        let context = NativeCallContext::new(
            hide_ip,
            ice,
            self.audio.clone(),
            self.video.clone(),
            Box::new(RemoteSink(self.remote.clone())),
        );
        self.manager.proceed(call_id, context, CallConfig::default(), None);
    }

    pub fn accept(&mut self, call_id: CallId) {
        self.manager.accept_call(call_id);
    }

    /// Ends the active call (or declines a ringing one).
    pub fn hangup(&mut self) {
        self.manager.hangup();
    }

    /// Drops a call without signaling (one we won't answer, say).
    pub fn drop_call(&mut self, call_id: CallId) {
        self.manager.drop_call(call_id);
    }

    pub fn received_offer(&mut self, peer: &str, call_id: CallId, offer: ReceivedOffer) {
        self.manager.received_offer(peer.to_owned(), call_id, offer);
    }

    pub fn received_answer(&mut self, peer: &str, call_id: CallId, answer: ReceivedAnswer) {
        self.manager.received_answer(peer.to_owned(), call_id, answer);
    }

    pub fn received_ice(&mut self, peer: &str, call_id: CallId, ice: ReceivedIce) {
        self.manager.received_ice(peer.to_owned(), call_id, ice);
    }

    pub fn received_hangup(&mut self, peer: &str, call_id: CallId, hangup: ReceivedHangup) {
        self.manager.received_hangup(peer.to_owned(), call_id, hangup);
    }

    pub fn received_busy(&mut self, peer: &str, call_id: CallId, busy: ReceivedBusy) {
        self.manager.received_busy(peer.to_owned(), call_id, busy);
    }

    /// Mutes or unmutes our microphone, and tells them.
    pub fn set_audio_enabled(&mut self, enabled: bool) {
        self.audio_on = enabled;
        self.audio.set_enabled(enabled);
        if let Ok(mut connection) = self.manager.active_connection() {
            let _ = connection.update_sender_status(signaling::SenderStatus { audio_enabled: Some(enabled), ..Default::default() });
        }
    }

    /// Turns our camera on or off (also for the lobby's preview), and tells
    /// them.
    pub fn set_video_enabled(&mut self, enabled: bool) {
        // While presenting, the share has the video; the camera follows
        // when it stops (see `stop_sharing`).
        if self.share.is_some() {
            return;
        }
        if enabled && self.camera.is_none() {
            let emit = self.emit.clone();
            let failed = move |error| emit(CallEvent::CameraFailed { error });
            match camera::Camera::start(self.source.clone(), self.local.clone(), self.camera_choice.clone(), failed) {
                Ok(camera) => self.camera = Some(camera),
                Err(error) => tracing::warn!(error = %format!("{error:#}"), "couldn't start the camera"),
            }
        } else if !enabled {
            self.camera = None;
            self.local.clear();
        }
        // What they'll see: on only if the camera actually started.
        let on = enabled && self.camera.is_some();
        self.video.set_enabled(on);
        if let Ok(mut connection) = self.manager.active_connection() {
            let _ = connection.update_sender_status(signaling::SenderStatus { video_enabled: Some(on), ..Default::default() });
        }
    }

    /// The microphones, speakers and cameras there are.
    pub fn devices(&mut self) -> Devices {
        let audio = |devices: ringrtc::common::Result<Vec<ringrtc::webrtc::peer_connection_factory::AudioDevice>>| {
            devices
                .unwrap_or_default()
                .into_iter()
                .map(|d| Device { id: d.unique_id, name: d.name })
                .collect::<Vec<_>>()
        };
        Devices {
            microphones: audio(self.factory.get_audio_recording_devices()),
            speakers: audio(self.factory.get_audio_playout_devices()),
            cameras: camera::list(),
        }
    }

    pub fn set_microphone(&mut self, id: &str) -> anyhow::Result<()> {
        self.factory.set_audio_recording_device_by_id(id).map_err(|e| anyhow!("{e}"))
    }

    pub fn set_speaker(&mut self, id: &str) -> anyhow::Result<()> {
        self.factory.set_audio_playout_device_by_id(id).map_err(|e| anyhow!("{e}"))
    }

    /// Chooses the camera (by name); a running one switches over.
    pub fn set_camera(&mut self, name: Option<String>) {
        self.camera_choice = name;
        if self.camera.take().is_some() {
            self.set_video_enabled(true);
        }
    }

    // ---------------------------------------------------------- group calls

    /// A client for `group_id`'s call (the group's identifier). Connect,
    /// then join; it asks for a membership proof and the members.
    pub fn create_group_call(&mut self, group_id: Vec<u8>) -> anyhow::Result<ClientId> {
        let params = ringrtc::core::call_manager::CreateGroupCallParams {
            group_id,
            sfu_url: SFU_URL.to_owned(),
            hkdf_extra_info: Vec::new(),
            audio_levels_interval: None,
            dred_duration: 0,
            svc_config: None,
            peer_connection_factory: None,
            outgoing_audio_track: self.audio.clone(),
            outgoing_video_track: self.video.clone(),
            incoming_video_sink: Some(Box::new(group::GroupSink(self.group_frames.clone()))),
        };
        self.manager.create_group_call_client(params).map_err(|e| anyhow!("{e}"))
    }

    pub fn group_connect(&mut self, client_id: ClientId) {
        self.manager.connect(client_id);
    }

    pub fn group_join(&mut self, client_id: ClientId) {
        self.manager.join(client_id);
    }

    /// Leaves and lets the client go.
    pub fn group_end(&mut self, client_id: ClientId) {
        self.manager.leave(client_id);
        self.manager.disconnect(client_id);
        self.manager.delete_group_call_client(client_id);
        self.group_frames.clear();
    }

    /// Rings everyone in the group (Desktop rings groups of up to 16).
    pub fn group_ring(&mut self, client_id: ClientId) {
        self.manager.group_ring(client_id, None);
    }

    pub fn set_membership_proof(&mut self, client_id: ClientId, proof: Vec<u8>) {
        self.manager.set_membership_proof(client_id, proof);
    }

    pub fn set_group_members(&mut self, client_id: ClientId, members: Vec<GroupMember>) {
        self.manager.set_group_members(client_id, members);
    }

    pub fn set_group_audio_muted(&mut self, client_id: ClientId, muted: bool) {
        self.manager.set_outgoing_audio_muted(client_id, muted);
    }

    pub fn set_group_video_muted(&mut self, client_id: ClientId, muted: bool) {
        self.manager.set_outgoing_video_muted(client_id, muted);
    }

    pub fn set_group_presenting(&mut self, client_id: ClientId, presenting: bool) {
        self.manager.set_presenting(client_id, presenting);
        self.manager.set_sharing_screen(client_id, presenting);
    }

    /// Sends a reaction (an emoji) to everyone in the call.
    pub fn group_react(&mut self, client_id: ClientId, value: String) {
        self.manager.react(client_id, value);
    }

    /// Raises or lowers our hand.
    pub fn group_raise_hand(&mut self, client_id: ClientId, raise: bool) {
        self.manager.raise_hand(client_id, raise);
    }

    /// Asks a participant's device to mute its microphone (Desktop's
    /// `sendRemoteMuteRequest`).
    pub fn group_remote_mute(&mut self, client_id: ClientId, target: DemuxId) {
        self.manager.send_remote_mute_request(client_id, target);
    }

    /// Mutes our microphone because `source` asked: RingRTC tells the
    /// others who did it.
    pub fn set_group_audio_muted_remotely(&mut self, client_id: ClientId, source: DemuxId) {
        self.audio_on = false;
        self.audio.set_enabled(false);
        self.manager.set_outgoing_audio_muted_remotely(client_id, source);
    }

    /// Which participants' video to receive, at what size.
    pub fn request_group_video(&mut self, client_id: ClientId, sizes: Vec<(DemuxId, u16, u16)>) {
        let requests = sizes
            .into_iter()
            .map(|(demux_id, width, height)| group_call::VideoRequest { demux_id, width, height, framerate: None })
            .collect();
        self.manager.request_video(client_id, requests, 0);
    }

    /// Declines (or, busy, turns away) a ring.
    pub fn cancel_group_ring(&mut self, group_id: Vec<u8>, ring_id: i64, reason: RingCancelReason) {
        let _ = self.manager.cancel_group_ring(group_id, ring_id.into(), Some(reason));
    }

    /// A group call's opaque `CallMessage` (rings, media keys).
    pub fn received_call_message(&mut self, sender: Vec<u8>, sender_device: DeviceId, our_device: DeviceId, data: Vec<u8>, age: Duration) {
        self.manager.received_call_message(sender, sender_device, our_device, data, age);
    }

    /// Who's in a group's call, without joining; answered with
    /// `GroupEvent::PeekResult`.
    pub fn peek_group_call(&mut self, request_id: u32, proof: Vec<u8>, members: Vec<GroupMember>) {
        self.manager.peek_group_call(request_id, SFU_URL.to_owned(), proof, members);
    }

    pub fn received_http_response(&mut self, request_id: u32, response: Option<HttpResponse>) {
        self.manager.received_http_response(request_id, response);
    }

    /// The displays and windows there are to share.
    pub fn share_sources() -> anyhow::Result<Vec<ShareSource>> {
        screen::sources()
    }

    /// Desktop's "Present": sends `source` instead of the camera (which
    /// stops), and tells them.
    pub fn start_sharing(&mut self, source: &str) -> anyhow::Result<()> {
        // (The camera stays until the share has started.)
        let share = screen::Share::start(source, self.source.clone(), self.local.clone())?;
        self.camera = None;
        self.share = Some(share);
        self.screenshare_mode(true);
        Ok(())
    }

    /// Stops presenting; `camera` turns the camera back on.
    pub fn stop_sharing(&mut self, camera: bool) {
        if self.share.take().is_none() {
            return;
        }
        self.local.clear();
        self.screenshare_mode(false);
        self.set_video_enabled(camera);
    }

    /// Tells them our microphone, camera and sharing state. Set before
    /// the call connected (in the lobby, say), it reached no one: what's
    /// sent goes over the call's connection.
    pub fn send_sender_status(&mut self) {
        let (video, sharing, audio) = (self.camera.is_some() || self.share.is_some(), self.share.is_some(), self.audio_on);
        if let Ok(mut connection) = self.manager.active_connection() {
            let _ = connection.update_sender_status(signaling::SenderStatus {
                video_enabled: Some(video),
                sharing_screen: Some(sharing),
                audio_enabled: Some(audio),
            });
        }
    }

    pub fn camera_on(&self) -> bool {
        self.camera.is_some()
    }

    pub fn is_sharing(&self) -> bool {
        self.share.is_some()
    }

    /// As Desktop's `setOutgoingVideoIsScreenShare`: the encoder's hint and
    /// size limits, the video on, and word to them.
    fn screenshare_mode(&mut self, on: bool) {
        self.video.set_content_hint(on);
        // A screen goes at full size; the camera at Desktop's 960×720, 30 fps.
        let (width, height, fps) = if on { (0, 0, 0) } else { CAMERA_LIMITS };
        self.source.adapt_output_format(width, height, fps);
        if on {
            self.video.set_enabled(true);
        }
        if let Ok(mut connection) = self.manager.active_connection() {
            let _ = connection.send_is_screenshare_update(on);
            let _ = connection.update_sender_status(signaling::SenderStatus {
                video_enabled: Some(on || self.camera.is_some()),
                sharing_screen: Some(on),
                ..Default::default()
            });
        }
    }

    /// After a call: the camera off and no stale frames.
    pub fn reset_media(&mut self) {
        if self.share.take().is_some() {
            self.screenshare_mode(false);
        }
        self.camera = None;
        self.local.clear();
        self.remote.clear();
        self.audio.set_enabled(true);
        self.audio_on = true;
    }
}

impl Drop for Calls {
    fn drop(&mut self) {
        let _ = self.manager.close();
    }
}

/// An ACI's 16 UUID bytes (RingRTC's `UserId`).
fn uuid_bytes(aci: &str) -> anyhow::Result<UserId> {
    let hex: String = aci.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(anyhow!("not an ACI: {aci}"));
    }
    (0..32).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| anyhow!("{e}"))).collect()
}

/// How old an offer is, from when the server got it (RingRTC turns away
/// stale ones).
pub fn offer_age(server_timestamp_ms: u64, now_ms: u64) -> Duration {
    Duration::from_millis(now_ms.saturating_sub(server_timestamp_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_become_bgra_and_the_self_view_is_mirrored() {
        // Two pixels: red, then blue.
        let rgba = [255, 0, 0, 255, 0, 0, 255, 255];
        assert_eq!(rgba_to_bgra(2, 1, &rgba, false), [0, 0, 255, 255, 255, 0, 0, 255]);
        assert_eq!(rgba_to_bgra(2, 1, &rgba, true), [255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn slots_number_their_frames() {
        let slot = FrameSlot::default();
        assert!(slot.latest().is_none());
        slot.put_rgba(1, 1, &[1, 2, 3, 4], false);
        slot.put_rgba(1, 1, &[5, 6, 7, 8], false);
        let frame = slot.latest().unwrap();
        assert_eq!((frame.seq, frame.bgra.as_slice()), (2, &[7, 6, 5, 8][..]));
        slot.clear();
        assert!(slot.latest().is_none());
    }

    #[test]
    fn acis_become_uuid_bytes() {
        let bytes = uuid_bytes("00000000-0000-4000-8000-00000000a11c").unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[14..], &[0xa1, 0x1c]);
        assert!(uuid_bytes("nope").is_err());
    }
}
