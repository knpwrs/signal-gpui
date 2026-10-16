use libsignal_protocol::SenderCertificate;

#[derive(Clone)]
pub struct UnidentifiedAccess {
    pub key: Vec<u8>,
    pub certificate: SenderCertificate,
    /// signal-gpui: a serialized `GroupSendFullToken` for this one
    /// recipient, sent as `Group-Send-Token` in place of the access key, as
    /// Desktop does for a group member we have no working access key for
    /// (`ts/util/getSendOptions.preload.ts`, `SEALED_SENDER.DISABLED`).
    pub group_send_token: Option<Vec<u8>>,
}
