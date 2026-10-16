//! signal-gpui: one message to many with sender keys, as Signal Desktop's
//! `sendToGroupViaSenderKey` sends it (`ts/util/sendToGroup.preload.ts`).
//!
//! We hand each recipient device our sender key once (a sender key
//! distribution message, sent to each person), then encrypt each message
//! once with it and send it to everyone in one sealed sender request
//! (`PUT /v1/messages/multi_recipient`). [`send_via_sender_key`] is Desktop's
//! loop: it starts over whenever it changed something (sessions, our key,
//! who has it), at most ten times. Who has our key ([`SenderKeyInfo`]) is
//! kept by the caller, per group or story list.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use libsignal_core::{Aci, DeviceId, ServiceId};
use libsignal_protocol::{
    create_sender_key_distribution_message, group_encrypt,
    process_prekey_bundle, sealed_sender_multi_recipient_encrypt,
    CiphertextMessageType, ContentHint, ProtocolAddress, ProtocolStore,
    SenderCertificate, SenderKeyStore, SessionRecord,
    SessionUsabilityRequirements, UnidentifiedSenderMessageContent,
};
use rand::rng;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;
use zkgroup::groups::GroupSecretParams;

use super::{MessageSender, MessageSenderError, SentMessage};
use crate::{
    groups_v2::endorsements::GroupSendEndorsements,
    push_service::{ServiceError, DEFAULT_DEVICE_ID},
    service_address::ServiceIdExt,
    session_store::SessionStoreExt,
    unidentified_access::UnidentifiedAccess,
    utils::{serde_device_id, serde_service_id},
    websocket::sender::MultiRecipientAuth,
};

/// How long a sender key is used before a new one (Desktop's
/// `MAX_SENDER_KEY_EXPIRE_DURATION`, its default for `desktop.senderKeyMaxAge`).
pub const SENDER_KEY_MAX_AGE_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// Desktop's `MAX_RECURSION`: how many times a send may start over.
pub const MAX_ATTEMPTS: usize = 10;

/// One recipient device that has (or is to get) our sender key. Desktop's
/// `DeviceType`: a device that re-registers gets a new registration id and
/// so counts as a new device.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SenderKeyDevice {
    #[serde(with = "serde_service_id")]
    pub service_id: ServiceId,
    #[serde(with = "serde_device_id")]
    pub device_id: DeviceId,
    pub registration_id: u32,
}

/// Desktop's `SenderKeyInfoType`, kept per group and per story list: the
/// distribution our key is for, when it was made, and which devices have
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderKeyInfo {
    pub distribution_id: Uuid,
    /// Milliseconds since the Unix epoch.
    pub created_at: u64,
    #[serde(default)]
    pub member_devices: Vec<SenderKeyDevice>,
}

impl SenderKeyInfo {
    pub fn new(now_ms: u64) -> Self {
        Self {
            distribution_id: Uuid::new_v4(),
            created_at: now_ms,
            member_devices: Vec::new(),
        }
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.created_at) > SENDER_KEY_MAX_AGE_MS
    }

    /// Desktop's `resetSenderKey`: nobody has our key any more. Desktop
    /// keeps the distribution id and deletes our key for it; presage's
    /// store can't delete a sender key, so a new distribution id (and so a
    /// new key) takes its place instead.
    pub fn reset(&mut self, now_ms: u64) {
        *self = Self::new(now_ms);
    }
}

/// What changed between who has our key and who we're sending to
/// (Desktop's `_analyzeSenderKeyDevices`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeviceChanges {
    pub new_devices: Vec<SenderKeyDevice>,
    pub new_service_ids: Vec<ServiceId>,
    pub removed_devices: Vec<SenderKeyDevice>,
    pub removed_service_ids: Vec<ServiceId>,
}

/// Desktop's `_analyzeSenderKeyDevices`: who's new to our key, and who had
/// it but isn't sent to now. A partial send (`partial`) reports no removals:
/// those left out of it keep our key.
pub fn analyze_sender_key_devices(
    member_devices: &[SenderKeyDevice],
    devices_for_send: &[SenderKeyDevice],
    partial: bool,
) -> DeviceChanges {
    let new_devices: Vec<SenderKeyDevice> = devices_for_send
        .iter()
        .filter(|d| !member_devices.contains(d))
        .cloned()
        .collect();
    let removed_devices: Vec<SenderKeyDevice> = member_devices
        .iter()
        .filter(|_| !partial)
        .filter(|d| !devices_for_send.contains(d))
        .cloned()
        .collect();
    DeviceChanges {
        new_service_ids: service_ids_of(&new_devices),
        removed_service_ids: service_ids_of(&removed_devices),
        new_devices,
        removed_devices,
    }
}

/// The distinct accounts among `devices`, in order.
fn service_ids_of(devices: &[SenderKeyDevice]) -> Vec<ServiceId> {
    let mut seen = HashSet::new();
    devices
        .iter()
        .map(|d| d.service_id)
        .filter(|s| seen.insert(*s))
        .collect()
}

/// Where a group message's `Group-Send-Token` comes from.
pub struct GroupSendAuth<'a> {
    pub endorsements: &'a GroupSendEndorsements,
    pub group_secret_params: GroupSecretParams,
    pub our_aci: Aci,
}

/// What a sender key send is for.
pub struct SenderKeyRequest<'a> {
    /// Everyone to send to (not us).
    pub recipients: Vec<ServiceId>,
    /// A story: no token needed, and distribution messages to a story list
    /// go as stories.
    pub story: bool,
    /// Whether it's for a group (a group message or a group story), whose
    /// distribution messages go as ordinary messages.
    pub group: bool,
    /// Required for a group message (not a story); without it the send
    /// fails over to one by one, as Desktop's does once
    /// `retireAccessKeyGroupSend` is on.
    pub auth: Option<GroupSendAuth<'a>>,
    /// Milliseconds since the Unix epoch.
    pub now_ms: u64,
    /// `recipients` are only some of the group or list (Desktop's
    /// `isPartialSend`, a retry to those an earlier send missed): those
    /// left out keep our key, and aren't taken to have left.
    pub partial: bool,
}

/// How a sender key send went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SenderKeyOutcome {
    /// Reached with the one sender key message.
    pub sent: Vec<ServiceId>,
    /// Left to send to one by one: no session could be started, no
    /// endorsement, or our key couldn't be given to them.
    pub normal: Vec<ServiceId>,
    /// The server says these aren't registered (any more).
    pub unregistered: Vec<ServiceId>,
}

/// The network and store work behind a sender key send, so the loop can be
/// tested without either.
#[allow(async_fn_in_trait)]
pub trait SenderKeyTransport {
    /// Desktop's `getOpenDevices`: the devices we have a usable session
    /// with, and the recipients we have none with.
    async fn open_devices(
        &mut self,
        recipients: &[ServiceId],
    ) -> Result<(Vec<SenderKeyDevice>, Vec<ServiceId>), MessageSenderError>;

    /// Fetches pre-keys and starts sessions with `devices` of `recipient`
    /// (`None`: all of them).
    async fn start_sessions(
        &mut self,
        recipient: &ServiceId,
        devices: Option<&[DeviceId]>,
    ) -> Result<(), MessageSenderError>;

    async fn archive_sessions(
        &mut self,
        recipient: &ServiceId,
        devices: &[DeviceId],
    ) -> Result<(), MessageSenderError>;

    /// Whether our own sender key for `distribution_id` is still stored.
    async fn has_sender_key(
        &mut self,
        distribution_id: Uuid,
    ) -> Result<bool, MessageSenderError>;

    /// Sends our key for `distribution_id` to each of `recipients`, as a
    /// story when `story`; one result each.
    async fn send_distribution_message(
        &mut self,
        recipients: &[ServiceId],
        distribution_id: Uuid,
        story: bool,
    ) -> Vec<(ServiceId, Result<(), MessageSenderError>)>;

    /// Encrypts the message with our key and sends it to every one of
    /// `devices` at once. Returns those the server says aren't registered.
    async fn send_sender_key_message(
        &mut self,
        devices: &[SenderKeyDevice],
        distribution_id: Uuid,
        auth: &MultiRecipientAuth,
    ) -> Result<Vec<ServiceId>, MessageSenderError>;
}

/// Desktop's `sendToGroupViaSenderKey`. `info` is updated as the send goes
/// (also when it fails part way), and the caller keeps it.
pub async fn send_via_sender_key<T: SenderKeyTransport>(
    transport: &mut T,
    info: &mut SenderKeyInfo,
    request: &SenderKeyRequest<'_>,
) -> Result<SenderKeyOutcome, MessageSenderError> {
    if !request.story && request.auth.is_none() {
        // Desktop: "missing groupSendToken and story=false. Failing over."
        return Err(MessageSenderError::MissingGroupSendToken);
    }
    let members: HashSet<ServiceId> =
        request.recipients.iter().copied().collect();
    // Started sessions with already, and couldn't give our key to.
    let mut fetched: HashSet<ServiceId> = HashSet::new();
    let mut unreachable: Vec<ServiceId> = Vec::new();
    let mut unregistered: Vec<ServiceId> = Vec::new();
    let mut skdm_failed: Vec<ServiceId> = Vec::new();

    for attempt in 0..=MAX_ATTEMPTS {
        debug!(attempt, "sender key send");
        // 1. Our key is too old: a new one.
        if info.is_expired(request.now_ms) {
            info!("sender key too old; resetting");
            info.reset(request.now_ms);
            continue;
        }

        // 2. The devices we'd send to.
        let left: Vec<ServiceId> = request
            .recipients
            .iter()
            .filter(|r| !unregistered.contains(r) && !skdm_failed.contains(r))
            .copied()
            .collect();
        let (devices, empty) = transport.open_devices(&left).await?;

        // 3. Start sessions with those we have none with, once each.
        let to_fetch: Vec<ServiceId> =
            empty.iter().filter(|s| fetched.insert(**s)).copied().collect();
        if !to_fetch.is_empty() {
            for recipient in &to_fetch {
                match transport.start_sessions(recipient, None).await {
                    Ok(()) => {},
                    Err(MessageSenderError::NotFound { .. }) => {
                        unregistered.push(*recipient);
                    },
                    Err(error) => return Err(error),
                }
            }
            continue;
        }
        // Still no session after fetching: one by one, where the reason
        // will be reported.
        for recipient in &empty {
            if !unreachable.contains(recipient)
                && !unregistered.contains(recipient)
            {
                unreachable.push(*recipient);
            }
        }

        // 4. Who can get the sender key message.
        let (for_sender_key, for_normal): (Vec<_>, Vec<_>) =
            devices.into_iter().partition(|d| {
                members.contains(&d.service_id)
                    && match &request.auth {
                        Some(auth) if !request.story => {
                            auth.endorsements.has_member(&d.service_id)
                        },
                        _ => true,
                    }
            });
        let sender_key_recipients = service_ids_of(&for_sender_key);
        let normal_recipients = service_ids_of(&for_normal);

        // 5. Desktop: "Not enough recipients for Sender Key message."
        if sender_key_recipients.len() < 2 {
            return Err(MessageSenderError::NotEnoughRecipients);
        }

        // 6, 7. Someone who had our key isn't a recipient any more: a new
        // key, sent to everyone.
        let changes =
            analyze_sender_key_devices(
                &info.member_devices,
                &for_sender_key,
                request.partial,
            );
        if changes
            .removed_service_ids
            .iter()
            .any(|s| !members.contains(s))
        {
            info!("a recipient left; resetting the sender key");
            info.reset(request.now_ms);
            continue;
        }

        // Our key went missing from the store: a new one.
        if !info.member_devices.is_empty()
            && !transport.has_sender_key(info.distribution_id).await?
        {
            warn!("our sender key is gone; resetting");
            info.reset(request.now_ms);
            continue;
        }

        // 8. New recipients or devices get our key first. Only those it
        // reached are recorded as having it.
        if !changes.new_service_ids.is_empty() {
            info!(
                count = changes.new_service_ids.len(),
                "sending our sender key"
            );
            // Desktop: a distribution message is a story only for a story
            // list.
            let results = transport
                .send_distribution_message(
                    &changes.new_service_ids,
                    info.distribution_id,
                    request.story && !request.group,
                )
                .await;
            for (recipient, result) in results {
                match result {
                    Ok(()) => info.member_devices.extend(
                        changes
                            .new_devices
                            .iter()
                            .filter(|d| d.service_id == recipient)
                            .cloned(),
                    ),
                    Err(MessageSenderError::NotFound { .. }) => {
                        unregistered.push(recipient)
                    },
                    Err(error) => {
                        warn!(%error, "couldn't send our sender key");
                        skdm_failed.push(recipient);
                    },
                }
            }
            continue;
        }

        // 9. Devices gone that don't need a new key.
        if !changes.removed_devices.is_empty() {
            info.member_devices
                .retain(|d| !changes.removed_devices.contains(d));
        }

        // 10. The sender key message.
        let auth = match &request.auth {
            Some(auth) if !request.story => {
                let recipients: HashSet<ServiceId> =
                    sender_key_recipients.iter().copied().collect();
                let token = auth
                    .endorsements
                    .build_token(
                        &recipients,
                        auth.our_aci,
                        &auth.group_secret_params,
                        request.now_ms / 1000,
                    )
                    .map_err(|e| MessageSenderError::Endorsement(e.to_string()))?;
                MultiRecipientAuth::GroupSendToken(token)
            },
            _ => MultiRecipientAuth::Story,
        };
        match transport
            .send_sender_key_message(
                &for_sender_key,
                info.distribution_id,
                &auth,
            )
            .await
        {
            Ok(not_registered) => {
                let sent = sender_key_recipients
                    .into_iter()
                    .filter(|s| !not_registered.contains(s))
                    .collect();
                unregistered.extend(not_registered);
                let mut normal = normal_recipients;
                for recipient in unreachable.into_iter().chain(skdm_failed) {
                    if !normal.contains(&recipient) {
                        normal.push(recipient);
                    }
                }
                return Ok(SenderKeyOutcome {
                    sent,
                    normal,
                    unregistered,
                });
            },
            Err(MessageSenderError::ServiceError(
                ServiceError::MultiRecipientMismatchedDevices(entries),
            )) => {
                // Desktop's `handle409Response`.
                for entry in entries {
                    if !entry.devices.missing_devices.is_empty() {
                        transport
                            .start_sessions(
                                &entry.uuid,
                                Some(&entry.devices.missing_devices),
                            )
                            .await?;
                    }
                    if !entry.devices.extra_devices.is_empty() {
                        transport
                            .archive_sessions(
                                &entry.uuid,
                                &entry.devices.extra_devices,
                            )
                            .await?;
                    }
                }
            },
            Err(MessageSenderError::ServiceError(
                ServiceError::MultiRecipientStaleDevices(entries),
            )) => {
                // Desktop's `handle410Response`: new sessions, and those
                // devices get our key again.
                for entry in entries {
                    let stale = &entry.devices.stale_devices;
                    if stale.is_empty() {
                        continue;
                    }
                    transport.archive_sessions(&entry.uuid, stale).await?;
                    transport.start_sessions(&entry.uuid, Some(stale)).await?;
                    info.member_devices.retain(|d| {
                        d.service_id != entry.uuid
                            || !stale.contains(&d.device_id)
                    });
                }
            },
            Err(error) => return Err(error),
        }
    }
    Err(MessageSenderError::SenderKeyAttempts)
}

impl MessageSenderError {
    /// Desktop's `_shouldFailSend`, inverted: whether a failed sender key
    /// send may go on one by one. Rate limits, challenges, server and
    /// network failures and untrusted identities fail the send; anything
    /// about the sender key itself (too few recipients, no token, a 401 or
    /// 404 from `multi_recipient`) falls over.
    pub fn sender_key_may_fall_over(&self) -> bool {
        match self {
            MessageSenderError::UntrustedIdentity { .. }
            | MessageSenderError::ProofRequired { .. } => false,
            MessageSenderError::ServiceError(error) => !matches!(
                error,
                ServiceError::RateLimitExceeded { .. }
                    | ServiceError::MessageTooLarge
                    | ServiceError::ProofRequiredError(_)
                    | ServiceError::ServerRejected
                    | ServiceError::Timeout { .. }
                    | ServiceError::SendError { .. }
                    | ServiceError::WsError(_)
                    | ServiceError::WsClosing { .. }
                    | ServiceError::UnhandledResponseCode { .. }
            ),
            _ => true,
        }
    }
}

/// The transport over a [`MessageSender`]: one message (`content`, an
/// encoded `Content`) for a group or story list.
pub(super) struct SenderTransport<'a, S> {
    pub(super) sender: &'a mut MessageSender<S>,
    pub(super) content: Vec<u8>,
    pub(super) timestamp: u64,
    pub(super) online: bool,
    pub(super) urgent: bool,
    pub(super) group_id: Option<Vec<u8>>,
    pub(super) content_hint: ContentHint,
    pub(super) certificate: SenderCertificate,
    /// For distribution messages that aren't stories.
    pub(super) access: HashMap<ServiceId, UnidentifiedAccess>,
}

impl<S> SenderTransport<'_, S>
where
    S: ProtocolStore + SenderKeyStore + SessionStoreExt + Sync + Clone,
{
    fn our_address(&self) -> ProtocolAddress {
        self.sender
            .local_aci
            .to_protocol_address(self.sender.device_id)
            .expect("valid device id")
    }
}

impl<S> SenderKeyTransport for SenderTransport<'_, S>
where
    S: ProtocolStore + SenderKeyStore + SessionStoreExt + Sync + Clone,
{
    async fn open_devices(
        &mut self,
        recipients: &[ServiceId],
    ) -> Result<(Vec<SenderKeyDevice>, Vec<ServiceId>), MessageSenderError>
    {
        let store = &self.sender.protocol_store;
        let mut devices = Vec::new();
        let mut empty = Vec::new();
        for recipient in recipients {
            let mut ids = vec![*DEFAULT_DEVICE_ID];
            ids.extend(store.get_sub_device_sessions(recipient).await?);
            ids.sort();
            ids.dedup();
            let before = devices.len();
            for device_id in ids {
                let address = recipient.to_protocol_address(device_id);
                let Some(session) = store.load_session(&address).await?
                else {
                    continue;
                };
                if !session
                    .has_usable_sender_chain(
                        SystemTime::now(),
                        SessionUsabilityRequirements::NotStale,
                    )
                    .unwrap_or(false)
                {
                    continue;
                }
                devices.push(SenderKeyDevice {
                    service_id: *recipient,
                    device_id,
                    registration_id: session.remote_registration_id()?,
                });
            }
            if devices.len() == before {
                empty.push(*recipient);
            }
        }
        Ok((devices, empty))
    }

    async fn start_sessions(
        &mut self,
        recipient: &ServiceId,
        devices: Option<&[DeviceId]>,
    ) -> Result<(), MessageSenderError> {
        let bundles = match devices {
            None => self
                .sender
                .identified_ws
                .get_pre_keys(recipient, *DEFAULT_DEVICE_ID)
                .await
                .map_err(|e| not_found(e, recipient))?,
            Some(devices) => {
                let mut bundles = Vec::new();
                for device in devices {
                    bundles.push(
                        self.sender
                            .identified_ws
                            .get_pre_key(recipient, *device)
                            .await
                            .map_err(|e| not_found(e, recipient))?,
                    );
                }
                bundles
            },
        };
        let local = self.our_address();
        let mut csprng = rng();
        for bundle in bundles {
            let address = recipient.to_protocol_address(bundle.device_id()?);
            process_prekey_bundle(
                &address,
                &local,
                &mut self.sender.protocol_store.clone(),
                &mut self.sender.protocol_store,
                &bundle,
                SystemTime::now(),
                &mut csprng,
            )
            .await
            .map_err(|error| match error {
                libsignal_protocol::SignalProtocolError::UntrustedIdentity(
                    _,
                ) => MessageSenderError::UntrustedIdentity {
                    address: *recipient,
                },
                error => error.into(),
            })?;
        }
        Ok(())
    }

    async fn archive_sessions(
        &mut self,
        recipient: &ServiceId,
        devices: &[DeviceId],
    ) -> Result<(), MessageSenderError> {
        for device in devices {
            self.sender
                .protocol_store
                .delete_service_addr_device_session(
                    &recipient.to_protocol_address(*device),
                )
                .await?;
        }
        Ok(())
    }

    async fn has_sender_key(
        &mut self,
        distribution_id: Uuid,
    ) -> Result<bool, MessageSenderError> {
        let ours = self.our_address();
        Ok(self
            .sender
            .protocol_store
            .load_sender_key(&ours, distribution_id)
            .await?
            .is_some())
    }

    async fn send_distribution_message(
        &mut self,
        recipients: &[ServiceId],
        distribution_id: Uuid,
        story: bool,
    ) -> Vec<(ServiceId, Result<(), MessageSenderError>)> {
        let ours = self.our_address();
        let skdm = match create_sender_key_distribution_message(
            &ours,
            distribution_id,
            &mut self.sender.protocol_store,
            &mut rng(),
        )
        .await
        {
            Ok(skdm) => skdm,
            Err(error) => {
                let reason = error.to_string();
                return recipients
                    .iter()
                    .map(|r| {
                        (
                            *r,
                            Err(MessageSenderError::ProtocolError(
                                libsignal_protocol::SignalProtocolError::InvalidState(
                                    "create_sender_key_distribution_message",
                                    reason.clone(),
                                ),
                            )),
                        )
                    })
                    .collect();
            },
        };
        let content = crate::proto::Content {
            sender_key_distribution_message: Some(skdm.serialized().to_vec()),
            ..Default::default()
        };
        // Desktop sends it under a timestamp of its own.
        let timestamp = chrono::Utc::now().timestamp_millis() as u64;
        let mut results = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            let access = if story {
                Some(UnidentifiedAccess {
                    key: Vec::new(),
                    certificate: self.certificate.clone(),
                    group_send_token: None,
                })
            } else {
                self.access.get(recipient).cloned()
            };
            let result = self
                .sender
                .try_send_content(
                    *recipient,
                    access.as_ref(),
                    content.clone(),
                    timestamp,
                    false,
                    story,
                )
                .await
                .map(|_| ());
            results.push((*recipient, result));
        }
        results
    }

    async fn send_sender_key_message(
        &mut self,
        devices: &[SenderKeyDevice],
        distribution_id: Uuid,
        auth: &MultiRecipientAuth,
    ) -> Result<Vec<ServiceId>, MessageSenderError> {
        let ours = self.our_address();
        let padded = crate::cipher::pad_message(&self.content);
        let message = group_encrypt(
            &mut self.sender.protocol_store,
            &ours,
            distribution_id,
            &padded,
            &mut rng(),
        )
        .await?;
        let usmc = UnidentifiedSenderMessageContent::new(
            CiphertextMessageType::SenderKey,
            self.certificate.clone(),
            message.serialized().to_vec(),
            self.content_hint,
            self.group_id.clone(),
        )?;
        let payload = multi_recipient_payload(
            &self.sender.protocol_store,
            devices,
            &usmc,
        )
        .await?;
        let response = self
            .sender
            .unidentified_ws
            .send_multi_recipient_message(
                payload,
                self.timestamp,
                auth,
                self.online,
                self.urgent,
            )
            .await?;
        Ok(response
            .uuids404
            .iter()
            .filter_map(|s| ServiceId::parse_from_service_id_string(s))
            .collect())
    }
}

/// Desktop's `encryptForSenderKey`, after `groupEncrypt`: the sealed sender
/// v2 payload for `devices`, sorted by account as Desktop sorts them.
pub async fn multi_recipient_payload<S>(
    store: &S,
    devices: &[SenderKeyDevice],
    usmc: &UnidentifiedSenderMessageContent,
) -> Result<Vec<u8>, MessageSenderError>
where
    S: ProtocolStore,
{
    let mut devices: Vec<&SenderKeyDevice> = devices.iter().collect();
    devices.sort_by(|a, b| {
        a.service_id
            .service_id_string()
            .cmp(&b.service_id.service_id_string())
            .then(a.device_id.cmp(&b.device_id))
    });
    let mut addresses = Vec::with_capacity(devices.len());
    let mut sessions: Vec<SessionRecord> = Vec::with_capacity(devices.len());
    for device in devices {
        let address = device.service_id.to_protocol_address(device.device_id)?;
        let session = store.load_session(&address).await?.ok_or_else(|| {
            libsignal_protocol::SignalProtocolError::SessionNotFound(
                libsignal_protocol::SessionNotFound::new(
                    address.clone(),
                    "sender key send",
                ),
            )
        })?;
        addresses.push(address);
        sessions.push(session);
    }
    let address_refs: Vec<&ProtocolAddress> = addresses.iter().collect();
    let session_refs: Vec<&SessionRecord> = sessions.iter().collect();
    Ok(sealed_sender_multi_recipient_encrypt(
        &address_refs,
        &session_refs,
        std::iter::empty::<ServiceId>(),
        usmc,
        store,
        &mut rng(),
    )
    .await?)
}

fn not_found(error: ServiceError, recipient: &ServiceId) -> MessageSenderError {
    match error {
        ServiceError::NotFoundError => MessageSenderError::NotFound {
            service_id: *recipient,
        },
        error => error.into(),
    }
}

/// The sync transcript's view of those reached by sender key: sealed, with
/// the identity key we have for them.
pub(super) async fn sent_messages<S: ProtocolStore>(
    store: &S,
    recipients: &[ServiceId],
) -> Vec<Result<SentMessage, MessageSenderError>> {
    let mut out = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let identity = store
            .get_identity(&recipient.to_protocol_address(*DEFAULT_DEVICE_ID))
            .await;
        out.push(match identity {
            Ok(Some(used_identity_key)) => Ok(SentMessage {
                recipient: *recipient,
                used_identity_key,
                unidentified: true,
                needs_sync: false,
            }),
            Ok(None) => Err(MessageSenderError::UntrustedIdentity {
                address: *recipient,
            }),
            Err(error) => Err(error.into()),
        });
    }
    out
}
