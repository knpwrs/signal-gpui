//! Group calls, through Signal's calling server (the SFU), as Desktop makes
//! them: RingRTC's group call client, fed a membership proof and the group's
//! members by the app, with each participant's video in its own frame slot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ringrtc::core::group_call::{self, ConnectionState, JoinState, RemoteDeviceState, RingUpdate};
use ringrtc::lite::sfu::{DemuxId, PeekInfo};
use ringrtc::webrtc::media::{VideoFrame, VideoSink};

use crate::FrameSlot;

/// Signal's calling server (Desktop's `sfuUrl`).
pub const SFU_URL: &str = "https://sfu.voip.signal.org";

/// Someone in a group call (one of their devices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub demux_id: DemuxId,
    /// Their ACI's 16 bytes.
    pub user_id: Vec<u8>,
    pub audio_muted: bool,
    pub video_muted: bool,
    pub presenting: bool,
    /// When they last were the main speaker (for ordering, as Desktop does).
    pub speaker_time: Option<SystemTime>,
    pub added_time: SystemTime,
}

impl From<&RemoteDeviceState> for Participant {
    fn from(state: &RemoteDeviceState) -> Self {
        let beat = &state.heartbeat_state;
        Self {
            demux_id: state.demux_id,
            user_id: state.user_id.clone(),
            audio_muted: beat.audio_muted.unwrap_or(false),
            video_muted: beat.video_muted.unwrap_or(true),
            presenting: beat.presenting.unwrap_or(false) || beat.sharing_screen.unwrap_or(false),
            speaker_time: state.speaker_time,
            added_time: state.added_time,
        }
    }
}

/// Who's in a group call, from a peek (before joining, or while in it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Peek {
    /// Their ACIs' bytes, one per device.
    pub users: Vec<Vec<u8>>,
    /// The call's era: changes each time it empties and starts again.
    pub era_id: Option<String>,
    pub max_devices: Option<u32>,
    /// Who started it (their ACI's bytes): Desktop's call history takes
    /// the call's direction and ringer from this.
    pub creator: Option<Vec<u8>>,
}

impl From<&PeekInfo> for Peek {
    fn from(info: &PeekInfo) -> Self {
        Self {
            users: info.devices.iter().filter_map(|d| d.user_id.clone()).collect(),
            era_id: info.era_id.clone(),
            max_devices: info.max_devices,
            creator: info.creator.clone(),
        }
    }
}

/// How a ring changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    /// They're ringing us.
    Requested,
    /// Another of our devices answered it.
    AcceptedElsewhere,
    /// It's over unanswered: expired, declined elsewhere, busy, or
    /// cancelled. (Desktop's call history counts each as missed:
    /// `ringUpdateToEvent` in `ts/util/callDisposition.preload.ts`.)
    Over,
}

impl From<RingUpdate> for Ring {
    fn from(update: RingUpdate) -> Self {
        match update {
            RingUpdate::Requested => Self::Requested,
            RingUpdate::AcceptedOnAnotherDevice => Self::AcceptedElsewhere,
            _ => Self::Over,
        }
    }
}

/// A group call's id for the call history, from its era (RingRTC's
/// `callIdFromEra`; a ring's id is the same number), its bits as an i64.
pub fn call_id_from_era(era: &str) -> i64 {
    group_call::RingId::from_era_id(era).into()
}

/// What a group call reports.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupEvent {
    /// Fetch a membership proof (`GET /v1/groups/token`) and hand it over.
    NeedsProof(group_call::ClientId),
    /// Hand over the group's members.
    NeedsMembers(group_call::ClientId),
    Connection { client_id: group_call::ClientId, connected: bool, connecting: bool },
    /// We're in (with our demux id), or not.
    Joined { client_id: group_call::ClientId, joined: Option<DemuxId> },
    Participants { client_id: group_call::ClientId, participants: Vec<Participant> },
    Peek { client_id: group_call::ClientId, peek: Peek },
    /// Answer to [`crate::Calls::peek_group_call`].
    PeekResult { request_id: u32, peek: Option<Peek> },
    Ended { client_id: group_call::ClientId },
    /// A ring for a group call: `group_id` is the group's identifier.
    Ring { group_id: Vec<u8>, ring_id: i64, sender: Vec<u8>, ring: Ring },
    /// Reactions sent in the call since the last, oldest first: who sent
    /// each (a demux id, ours included) and the emoji.
    Reactions { client_id: group_call::ClientId, reactions: Vec<(DemuxId, String)> },
    /// Whose hands are up (ours included), in the order they went up.
    RaisedHands { client_id: group_call::ClientId, hands: Vec<DemuxId> },
    /// Someone (`source`) muted our microphone.
    MutedBy { client_id: group_call::ClientId, source: DemuxId },
    /// Someone muted someone else (or us on another device).
    ObservedMute { client_id: group_call::ClientId, source: DemuxId, target: DemuxId },
}

pub(crate) fn connection(state: &ConnectionState) -> (bool, bool) {
    match state {
        ConnectionState::Connected => (true, false),
        ConnectionState::Connecting | ConnectionState::Reconnecting => (false, true),
        ConnectionState::NotConnected => (false, false),
    }
}

pub(crate) fn joined(state: &JoinState) -> Option<DemuxId> {
    match state {
        JoinState::Joined(id) | JoinState::Pending(id) => Some(*id),
        JoinState::NotJoined(_) | JoinState::Joining => None,
    }
}

/// Every participant's latest frame, by demux id.
#[derive(Clone, Default)]
pub struct GroupFrames(Arc<Mutex<HashMap<DemuxId, FrameSlot>>>);

impl GroupFrames {
    pub fn slot(&self, demux_id: DemuxId) -> FrameSlot {
        self.0.lock().map(|mut map| map.entry(demux_id).or_default().clone()).unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.0.lock() {
            map.clear();
        }
    }
}

#[derive(Clone)]
pub(crate) struct GroupSink(pub GroupFrames);

impl VideoSink for GroupSink {
    fn on_video_frame(&self, demux_id: DemuxId, frame: VideoFrame) {
        let (width, height) = (frame.width(), frame.height());
        let mut rgba = vec![0; width as usize * height as usize * 4];
        if frame.to_rgba(&mut rgba) {
            self.0.slot(demux_id).put_rgba(width, height, &rgba, false);
        }
    }

    fn box_clone(&self) -> Box<dyn VideoSink> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_ids_come_from_eras_as_ringrtcs_do() {
        // Sixteen hex digits are the id itself (RingRTC's usual case).
        assert_eq!(call_id_from_era("00000000000000ff"), 255);
        assert_eq!(call_id_from_era("ffffffffffffffff"), -1);
        // Anything else is hashed, the same each time.
        assert_eq!(call_id_from_era("not hex"), call_id_from_era("not hex"));
        assert_ne!(call_id_from_era("not hex"), call_id_from_era("other"));
    }

    #[test]
    fn each_participant_has_a_slot() {
        let frames = GroupFrames::default();
        frames.slot(7).put_rgba(1, 1, &[1, 2, 3, 4], false);
        assert!(frames.slot(7).latest().is_some());
        assert!(frames.slot(8).latest().is_none());
        frames.clear();
        assert!(frames.slot(7).latest().is_none());
    }
}
