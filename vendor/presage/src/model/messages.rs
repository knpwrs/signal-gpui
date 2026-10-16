use libsignal_service::{prelude::Content, protocol::ServiceId};

#[derive(Debug)]
pub enum Received {
    /// when the receive loop is empty, happens when opening the websocket for the first time
    /// once you're done synchronizing all pending messages for this registered client.
    QueueEmpty,

    /// Got contacts (only applies if linked to a primary device
    /// Contacts can be later queried in the store.
    Contacts,

    // Failed to receive a message from the specified service ID and device.
    // A session reset with the contact may fix this.
    // signal-gpui: with the sender's device; only for messages we couldn't
    // decrypt (see `RetryRequest`).
    DecryptionError(ServiceId, u32),

    // signal-gpui: the specified service ID couldn't decrypt one of ours
    // and asks for it again (a `DecryptionErrorMessage`). Upstream reports
    // this as a `DecryptionError` too.
    RetryRequest(ServiceId),

    /// Incoming decrypted message with metadata and content
    Content(Box<Content>),
}
