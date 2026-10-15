//! Calls on the wire: RingRTC's signaling as Signal `CallMessage`s (as
//! Desktop's `ts/services/calling` sends and reads them), and the TURN
//! relays a call goes through.

use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::push_service::HttpAuthOverride;
use presage::proto::{CallMessage, call_message};
use signal_calls::{CallId, CallMediaType, DeviceId, Hangup, HangupType, Message, Relay};

use crate::actor::SignalManager;

/// RingRTC's message as a `CallMessage` (to one device, or all).
pub(crate) fn to_call_message(call_id: CallId, device: Option<DeviceId>, message: &Message) -> CallMessage {
    let id = Some(call_id.as_u64());
    let mut out = CallMessage { destination_device_id: device, ..Default::default() };
    match message {
        Message::Offer(offer) => {
            let kind = match offer.call_media_type {
                CallMediaType::Audio => call_message::offer::Type::OfferAudioCall,
                CallMediaType::Video => call_message::offer::Type::OfferVideoCall,
            };
            out.offer = Some(call_message::Offer { id, r#type: Some(kind.into()), opaque: Some(offer.opaque.clone()) });
        }
        Message::Answer(answer) => {
            out.answer = Some(call_message::Answer { id, opaque: Some(answer.opaque.clone()) });
        }
        Message::Ice(ice) => {
            out.ice_update = ice
                .candidates
                .iter()
                .map(|c| call_message::IceUpdate { id, opaque: Some(c.opaque.clone()) })
                .collect();
        }
        Message::Hangup(hangup) => {
            let (kind, device_id) = hangup.to_type_and_device_id();
            out.hangup = Some(call_message::Hangup { id, r#type: Some(kind as i32), device_id });
        }
        Message::Busy => out.busy = Some(call_message::Busy { id }),
    }
    out
}

/// A received `CallMessage`, for RingRTC.
#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    Offer { call_id: CallId, video: bool, opaque: Vec<u8> },
    Answer { call_id: CallId, opaque: Vec<u8> },
    Ice { call_id: CallId, candidates: Vec<Vec<u8>> },
    Hangup { call_id: CallId, hangup: Hangup },
    Busy { call_id: CallId },
    /// A group call's message (a ring, media keys).
    Opaque { data: Vec<u8> },
}

pub(crate) fn from_call_message(message: &CallMessage) -> Vec<Incoming> {
    let mut out = Vec::new();
    if let Some(offer) = &message.offer
        && let (Some(id), Some(opaque)) = (offer.id, &offer.opaque)
    {
        let video = offer.r#type() == call_message::offer::Type::OfferVideoCall;
        out.push(Incoming::Offer { call_id: CallId::new(id), video, opaque: opaque.clone() });
    }
    if let Some(answer) = &message.answer
        && let (Some(id), Some(opaque)) = (answer.id, &answer.opaque)
    {
        out.push(Incoming::Answer { call_id: CallId::new(id), opaque: opaque.clone() });
    }
    // One message carries a batch of candidates for one call.
    if let Some(id) = message.ice_update.iter().find_map(|u| u.id) {
        let candidates: Vec<Vec<u8>> =
            message.ice_update.iter().filter(|u| u.id == Some(id)).filter_map(|u| u.opaque.clone()).collect();
        if !candidates.is_empty() {
            out.push(Incoming::Ice { call_id: CallId::new(id), candidates });
        }
    }
    if let Some(hangup) = &message.hangup
        && let Some(id) = hangup.id
    {
        let kind = HangupType::from_i32(hangup.r#type.unwrap_or(0)).unwrap_or(HangupType::Normal);
        let hangup = Hangup::from_type_and_device_id(kind, hangup.device_id.unwrap_or(0));
        out.push(Incoming::Hangup { call_id: CallId::new(id), hangup });
    }
    if let Some(busy) = &message.busy
        && let Some(id) = busy.id
    {
        out.push(Incoming::Busy { call_id: CallId::new(id) });
    }
    if let Some(data) = message.opaque.as_ref().and_then(|o| o.data.clone()) {
        out.push(Incoming::Opaque { data });
    }
    out
}

/// Signal's TURN relays for a call (`GET /v2/calling/relays`), as Desktop
/// lists them: each relay by IP (with its hostname, for TLS) and by name.
pub(crate) async fn relays(manager: &SignalManager) -> anyhow::Result<Vec<Relay>> {
    let service = crate::storage::push_service(manager)?;
    let response = service
        .request(reqwest::Method::GET, Endpoint::service("/v2/calling/relays"), HttpAuthOverride::NoOverride)?
        .send()
        .await?
        .error_for_status()?;
    let json: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    Ok(parse_relays(&json))
}

fn parse_relays(json: &serde_json::Value) -> Vec<Relay> {
    let strings = |value: &serde_json::Value| -> Vec<String> {
        value.as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect()).unwrap_or_default()
    };
    let mut out = Vec::new();
    for relay in json["relays"].as_array().into_iter().flatten() {
        let username = relay["username"].as_str().unwrap_or_default().to_owned();
        let password = relay["password"].as_str().unwrap_or_default().to_owned();
        let hostname = relay["hostname"].as_str().unwrap_or_default().to_owned();
        let by_ip = strings(&relay["urlsWithIps"]);
        if !by_ip.is_empty() {
            out.push(Relay { username: username.clone(), password: password.clone(), hostname, urls: by_ip });
        }
        let by_name = strings(&relay["urls"]);
        if !by_name.is_empty() {
            out.push(Relay { username, password, hostname: String::new(), urls: by_name });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use signal_calls::{Answer, Ice, IceCandidate, Offer};

    #[test]
    fn signaling_round_trips_through_call_messages() {
        let id = CallId::new(42);
        // (The opaque parts are protobufs; an empty one is valid.)
        let offer = Message::Offer(Offer::new(CallMediaType::Video, Vec::new()).unwrap());
        let wire = to_call_message(id, None, &offer);
        assert_eq!(from_call_message(&wire), [Incoming::Offer { call_id: id, video: true, opaque: Vec::new() }]);

        let answer = Message::Answer(Answer::new(Vec::new()).unwrap());
        let wire = to_call_message(id, Some(2), &answer);
        assert_eq!(wire.destination_device_id, Some(2));
        assert_eq!(from_call_message(&wire), [Incoming::Answer { call_id: id, opaque: Vec::new() }]);

        let ice = Message::Ice(Ice { candidates: vec![IceCandidate::new(vec![4]), IceCandidate::new(vec![5])] });
        let wire = to_call_message(id, None, &ice);
        assert_eq!(from_call_message(&wire), [Incoming::Ice { call_id: id, candidates: vec![vec![4], vec![5]] }]);

        let hangup = Message::Hangup(Hangup::AcceptedOnAnotherDevice(3));
        let wire = to_call_message(id, None, &hangup);
        assert_eq!(from_call_message(&wire), [Incoming::Hangup { call_id: id, hangup: Hangup::AcceptedOnAnotherDevice(3) }]);

        let wire = to_call_message(id, None, &Message::Busy);
        assert_eq!(from_call_message(&wire), [Incoming::Busy { call_id: id }]);

        let opaque = CallMessage {
            opaque: Some(call_message::Opaque { data: Some(vec![7]), urgency: None }),
            ..Default::default()
        };
        assert_eq!(from_call_message(&opaque), [Incoming::Opaque { data: vec![7] }]);
    }

    #[test]
    fn relays_are_listed_by_ip_then_by_name() {
        let json = serde_json::json!({ "relays": [{
            "username": "u", "password": "p", "hostname": "turn.example",
            "urls": ["turn:turn.example"], "urlsWithIps": ["turn:192.0.2.1"], "ttl": 86400
        }]});
        let relays = parse_relays(&json);
        assert_eq!(relays.len(), 2);
        assert_eq!((relays[0].hostname.as_str(), relays[0].urls[0].as_str()), ("turn.example", "turn:192.0.2.1"));
        assert_eq!((relays[1].hostname.as_str(), relays[1].urls[0].as_str()), ("", "turn:turn.example"));
    }
}
