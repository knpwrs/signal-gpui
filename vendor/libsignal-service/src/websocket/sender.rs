use crate::{
    push_service::response::error_mapper,
    sender::{OutgoingPushMessages, SendMessageResponse},
    unidentified_access::UnidentifiedAccess,
    utils::BASE64_RELAXED,
};

use super::*;
use base64::Engine;

// Signal-Server: controllers/MessageController.java:198
// (PUT /v1/messages/{destination})
error_mapper! {
    put_messages_errors:
        // 409: mismatched devices, MessageController.java:426
        CONFLICT => MismatchedDevicesException(crate::push_service::MismatchedDevices),
        // 410: stale devices, MessageController.java:421
        GONE => StaleDevices(crate::push_service::StaleDevices),
        // 413: message too large, MessageController.java:433
        PAYLOAD_TOO_LARGE => MessageTooLarge,
        // 404: unregistered recipient, MessageController.java:316
        NOT_FOUND => UnregisteredRecipient,
        // 428: proof required, MessageController.java:365
        PRECONDITION_REQUIRED => ProofRequiredError(crate::push_service::ProofRequired),
}

// Signal-Server: controllers/MessageController.java:466
// (PUT /v1/messages/multi_recipient)
error_mapper! {
    put_multi_recipient_messages_errors:
        // 409: mismatched devices, MessageController.java:670
        CONFLICT => MultiRecipientMismatchedDevices(Vec<crate::push_service::AccountMismatchedDevices>),
        // 410: stale devices, MessageController.java:685
        GONE => MultiRecipientStaleDevices(Vec<crate::push_service::AccountStaleDevices>),
        // 413: message too large, MessageController.java:660
        PAYLOAD_TOO_LARGE => MessageTooLarge,
        // 428: proof required, MessageController.java:638
        PRECONDITION_REQUIRED => ProofRequiredError(crate::push_service::ProofRequired),
}

impl<C: WebSocketType> SignalWebSocket<C> {
    pub async fn send_messages(
        &mut self,
        messages: OutgoingPushMessages,
    ) -> Result<SendMessageResponse, ServiceError> {
        let request = WebSocketRequestMessage::new(Method::PUT)
            .path(format!(
                "/v1/messages/{}",
                messages.destination.service_id_string()
            ))
            .json(&messages)?;
        self.request_json_with(request, put_messages_errors).await
    }

    /// signal-gpui: a story (or group story reply), sealed, flagged as a
    /// story, and without an access key (Signal-Server's
    /// `MessageController`: stories need no access).
    pub async fn send_messages_story(
        &mut self,
        messages: OutgoingPushMessages,
    ) -> Result<SendMessageResponse, ServiceError> {
        let request = WebSocketRequestMessage::new(Method::PUT)
            .path(format!(
                "/v1/messages/{}?story=true",
                messages.destination.service_id_string()
            ))
            .json(&messages)?;
        self.request_json_with(request, put_messages_errors).await
    }

    pub async fn send_messages_unidentified(
        &mut self,
        messages: OutgoingPushMessages,
        access: &UnidentifiedAccess,
    ) -> Result<SendMessageResponse, ServiceError> {
        let builder = WebSocketRequestMessage::new(Method::PUT).path(format!(
            "/v1/messages/{}",
            messages.destination.service_id_string()
        ));
        // signal-gpui: a group send endorsement stands in for the access
        // key (libsignal-net's `UserBasedSendAuthorization`).
        let builder = match &access.group_send_token {
            Some(token) => builder
                .header("Group-Send-Token", BASE64_RELAXED.encode(token)),
            None => builder.header(
                "Unidentified-Access-Key",
                BASE64_RELAXED.encode(&access.key),
            ),
        };
        let request = builder.json(&messages)?;
        self.request_json_with(request, put_messages_errors).await
    }

    /// signal-gpui: one sealed sender message for many recipients
    /// (`PUT /v1/messages/multi_recipient`), as libsignal-net's
    /// `send_multi_recipient_message` sends it. Returns the recipients the
    /// server says aren't registered.
    pub async fn send_multi_recipient_message(
        &mut self,
        payload: Vec<u8>,
        timestamp: u64,
        auth: &MultiRecipientAuth,
        online: bool,
        urgent: bool,
    ) -> Result<MultiRecipientResponse, ServiceError> {
        let request =
            multi_recipient_request(payload, timestamp, auth, online, urgent);
        self.request_json_with(request, put_multi_recipient_messages_errors)
            .await
    }
}

/// signal-gpui: how a multi-recipient send is authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiRecipientAuth {
    /// A story: no authorization, and the story flag.
    Story,
    /// A group message: a serialized `GroupSendFullToken` for every
    /// recipient, from the group's send endorsements.
    GroupSendToken(Vec<u8>),
}

/// signal-gpui: the server's answer to a multi-recipient send.
#[derive(Debug, Default, serde::Deserialize)]
pub struct MultiRecipientResponse {
    /// Recipients that aren't registered (service id strings).
    #[serde(default)]
    pub uuids404: Vec<String>,
}

/// The content type of a multi-recipient message.
pub const MULTI_RECIPIENT_CONTENT_TYPE: &str =
    "application/vnd.signal-messenger.mrm";

/// signal-gpui: the request libsignal-net makes for a multi-recipient send
/// (`rust/net/chat/src/ws/messages.rs`): the query carries the timestamp,
/// `online` and `urgent`, and `story=true` for a story; a group message
/// carries its token, base64, in `Group-Send-Token`.
pub fn multi_recipient_request(
    payload: Vec<u8>,
    timestamp: u64,
    auth: &MultiRecipientAuth,
    online: bool,
    urgent: bool,
) -> WebSocketRequestMessage {
    let story = if matches!(auth, MultiRecipientAuth::Story) {
        "&story=true"
    } else {
        ""
    };
    let builder = WebSocketRequestMessage::new(Method::PUT).path(format!(
        "/v1/messages/multi_recipient?ts={timestamp}&online={online}&urgent={urgent}{story}"
    ));
    let builder = match auth {
        MultiRecipientAuth::Story => builder,
        MultiRecipientAuth::GroupSendToken(token) => {
            builder.header("Group-Send-Token", BASE64_RELAXED.encode(token))
        },
    };
    builder.body(MULTI_RECIPIENT_CONTENT_TYPE, payload)
}
