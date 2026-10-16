//! Message history at link time ("link and sync"), as Signal Desktop does
//! it: the phone uploads an encrypted backup for the new device, which
//! fetches it, decrypts it with the one-time key from linking, and imports
//! chats and messages.
//!
//! The file: a 16-byte IV, AES-256-CBC ciphertext and an HMAC-SHA256 (the
//! attachment format), wrapping gzip, wrapping length-prefixed protobufs: a
//! `BackupInfo`, then `Frame`s (recipients, chats, then chat items).
//!
//! Attachments come over as locators (where each file is and its key); the
//! files are fetched after the import, from the transit tier or the backup
//! (media) tier (`backup_media.rs`).

use std::collections::HashMap;
use std::io::Read as _;

use anyhow::{Context as _, anyhow, bail};
use libsignal_message_backup::proto::backup as proto;
use presage::libsignal_service::protocol::Aci;
use protobuf::Message as _;
use signal_store::{
    AppDb, AttachmentMeta, ChatState, ConversationId, ConversationKind, ConversationUpdate, DeliveryStatus, Direction,
    LinkPreview, NewMessage, Quote, Reaction, RemoteAttachment,
};

/// What the phone did with the history transfer.
pub(crate) enum Archive {
    /// Uploaded: where to download it.
    Ready { cdn: u32, key: String },
    /// The phone chose to skip it.
    Skipped,
}

/// Waits for the phone to upload the history (Desktop's
/// `getTransferArchive`): long polls, up to an hour.
pub(crate) async fn wait_for_archive(manager: &crate::actor::SignalManager) -> anyhow::Result<Archive> {
    use presage::libsignal_service::configuration::Endpoint;
    use presage::libsignal_service::push_service::HttpAuthOverride;
    let service = crate::storage::push_service(manager)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60 * 60);
    while std::time::Instant::now() < deadline {
        let response = service
            .request(
                reqwest::Method::GET,
                Endpoint::service("/v1/devices/transfer_archive?timeout=300"),
                HttpAuthOverride::NoOverride,
            )?
            .timeout(std::time::Duration::from_secs(315))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            // A dropped long poll: ask again.
            Err(error) if error.is_timeout() => continue,
            Err(error) => return Err(error.into()),
        };
        match response.status().as_u16() {
            204 => continue,
            200 => {
                let json: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
                return match (json["cdn"].as_u64(), json["key"].as_str(), json["error"].as_str()) {
                    (Some(cdn), Some(key), _) => Ok(Archive::Ready { cdn: cdn as u32, key: key.to_owned() }),
                    (_, _, Some("CONTINUE_WITHOUT_UPLOAD")) => Ok(Archive::Skipped),
                    (_, _, Some("RELINK_REQUESTED")) => bail!("your phone asked to link again"),
                    _ => bail!("unexpected transfer answer"),
                };
            }
            status => bail!("the server answered {status}"),
        }
    }
    bail!("timed out waiting for your phone")
}

/// Downloads the uploaded history from the CDN.
pub(crate) async fn download(
    manager: &crate::actor::SignalManager,
    cdn: u32,
    key: &str,
    mut progress: impl FnMut(u64, Option<u64>),
) -> anyhow::Result<Vec<u8>> {
    use presage::libsignal_service::configuration::Endpoint;
    use presage::libsignal_service::push_service::HttpAuthOverride;
    let service = crate::storage::push_service(manager)?;
    let key: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
    let response = service
        .request(reqwest::Method::GET, Endpoint::cdn(cdn, format!("attachments/{key}")), HttpAuthOverride::Unidentified)?
        .send()
        .await?
        .error_for_status()?;
    let total = response.content_length();
    // Trust the length for preallocating only so far.
    let mut data = Vec::with_capacity(total.unwrap_or(0).min(256 << 20) as usize);
    let mut response = response;
    progress(0, total);
    while let Some(chunk) = response.chunk().await? {
        data.extend_from_slice(&chunk);
        progress(data.len() as u64, total);
    }
    Ok(data)
}

/// Downloads a transferred attachment from the transit tier, checks it
/// against its digest or plaintext hash, and decrypts it. `size` (when
/// known) trims the padding.
/// An attachment that's no longer on Signal's servers: trying again won't
/// help.
#[derive(Debug)]
pub(crate) struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("it's no longer on Signal's servers")
    }
}

impl std::error::Error for Gone {}

pub(crate) async fn download_attachment(
    manager: &crate::actor::SignalManager,
    remote: &RemoteAttachment,
    size: u64,
) -> anyhow::Result<Vec<u8>> {
    use presage::libsignal_service::configuration::Endpoint;
    use presage::libsignal_service::push_service::HttpAuthOverride;
    use sha2::Digest as _;
    let key: [u8; 64] = hex::decode(&remote.key)?.try_into().map_err(|_| anyhow!("bad attachment key"))?;
    let service = crate::storage::push_service(manager)?;
    let cdn_key = remote.cdn_key.as_deref().context("it isn't on the transit tier")?;
    let path = format!("attachments/{cdn_key}");
    let response = service.request(reqwest::Method::GET, Endpoint::cdn(remote.cdn, path), HttpAuthOverride::Unidentified)?.send().await?;
    // Desktop's `AttachmentUndownloadableFromTransitTierError`
    // (`ts/util/downloadAttachment.preload.ts`): a 404, or a 403 from CDN 0.
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || (status == reqwest::StatusCode::FORBIDDEN && remote.cdn == 0) {
        return Err(Gone.into());
    }
    let mut data = response.error_for_status()?.bytes().await?.to_vec();
    if let Some(digest) = &remote.digest
        && sha2::Sha256::digest(&data).as_slice() != hex::decode(digest)?.as_slice()
    {
        bail!("the download doesn't match its digest");
    }
    let mut data = tokio::task::spawn_blocking(move || {
        presage::libsignal_service::attachment_cipher::decrypt_in_place(key, &mut data).map(|()| data)
    })
    .await?
    .map_err(|e| anyhow!("can't decrypt the attachment: {e:?}"))?;
    if size > 0 && (size as usize) < data.len() {
        data.truncate(size as usize);
    }
    if let Some(hash) = &remote.plaintext_hash
        && sha2::Sha256::digest(&data).as_slice() != hex::decode(hash)?.as_slice()
    {
        bail!("the download doesn't match its hash");
    }
    Ok(data)
}

/// Verifies, decrypts and decompresses a transferred backup.
pub(crate) fn decrypt(mut data: Vec<u8>, ephemeral_key: [u8; 32], aci: Aci) -> anyhow::Result<Vec<u8>> {
    use libsignal_account_keys::BackupKey;
    use libsignal_message_backup::key::MessageBackupKey;
    let backup_key: BackupKey = BackupKey(ephemeral_key);
    let backup_id = backup_key.derive_backup_id(&aci);
    let key = MessageBackupKey::derive(&backup_key, &backup_id, None);
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(&key.aes_key);
    combined[32..].copy_from_slice(&key.hmac_key);
    if data.len() < 48 {
        bail!("backup too short");
    }
    presage::libsignal_service::attachment_cipher::decrypt_in_place(combined, &mut data)
        .map_err(|e| anyhow!("can't decrypt the backup: {e:?}"))?;
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(data.as_slice()).read_to_end(&mut plain).context("decompressing the backup")?;
    Ok(plain)
}

/// Splits length-prefixed protobufs.
fn delimited(mut bytes: &[u8]) -> impl Iterator<Item = anyhow::Result<&[u8]>> {
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        let mut len = 0usize;
        let mut shift = 0;
        loop {
            let Some((&byte, rest)) = bytes.split_first() else { return Some(Err(anyhow!("truncated length"))) };
            bytes = rest;
            len |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return Some(Err(anyhow!("bad length")));
            }
        }
        if bytes.len() < len {
            return Some(Err(anyhow!("truncated frame")));
        }
        let (frame, rest) = bytes.split_at(len);
        bytes = rest;
        Some(Ok(frame))
    })
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Imported {
    pub chats: usize,
    pub messages: usize,
    /// Attachments to fetch now, as `(message id, attachment index)`: photos
    /// and GIFs whole, other images and videos that may be in the backup
    /// tier as thumbnails only (the actor's `download_transferred`); other
    /// files wait to be opened.
    pub downloads: Vec<(i64, usize)>,
    /// Messages whose whole text is still to download.
    pub long_texts: Vec<i64>,
    /// Installed sticker packs: `(pack id, pack key)`.
    pub sticker_packs: Vec<(Vec<u8>, Vec<u8>)>,
    /// The account's privacy settings, when the backup has them.
    pub settings: Option<crate::AccountSettings>,
}

enum Who {
    Me,
    Contact { sid: String, name: String, e164: Option<String>, blocked: bool },
    /// `ended`: an admin ended it for everyone (`GroupSnapshot.terminated`).
    Group { key: Vec<u8>, title: String, members: Vec<String>, blocked: bool, ended: bool },
    Other,
}

/// A contact's verified key transparency data, kept as Desktop's backup
/// import keeps it (`Contact.keyTransparencyData`, with an ACI).
async fn import_key_transparency(db: &AppDb, recipient: &proto::Recipient) {
    use proto::recipient::Destination;
    let Some(Destination::Contact(contact)) = &recipient.destination else { return };
    let Some(data) = contact.keyTransparencyData.as_deref().filter(|d| !d.is_empty()) else { return };
    let Some(aci) = contact.aci.as_deref().and_then(|b| <[u8; 16]>::try_from(b).ok()).map(Aci::from_uuid_bytes) else {
        return;
    };
    if let Err(error) = crate::key_transparency::import_account_data(db, &aci, data).await {
        tracing::warn!(%error, "failed to save key transparency data");
    }
}

fn uuid_sid(bytes: &[u8]) -> Option<String> {
    let bytes: [u8; 16] = bytes.try_into().ok()?;
    Some(Aci::from_uuid_bytes(bytes).service_id_string())
}

fn join(given: &str, family: &str) -> Option<String> {
    let name = format!("{} {}", given.trim(), family.trim());
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Signal's name order: nickname, address-book name, profile name, number.
fn contact_name(contact: &proto::Contact) -> String {
    let nickname = contact.nickname.as_ref().and_then(|n| join(&n.given, &n.family));
    let system = Some(contact.systemNickname.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| join(&contact.systemGivenName, &contact.systemFamilyName));
    let profile = join(contact.profileGivenName.as_deref().unwrap_or(""), contact.profileFamilyName.as_deref().unwrap_or(""));
    nickname
        .or(system)
        .or(profile)
        .or_else(|| contact.e164.map(|e| format!("+{e}")))
        .or_else(|| contact.username.clone())
        .unwrap_or_else(|| "Unknown".into())
}

fn who(recipient: &proto::Recipient) -> Who {
    use proto::recipient::Destination;
    match &recipient.destination {
        Some(Destination::Self_(_)) => Who::Me,
        Some(Destination::Contact(contact)) => match contact.aci.as_deref().and_then(uuid_sid) {
            Some(sid) => Who::Contact {
                sid,
                name: contact_name(contact),
                e164: contact.e164.map(|e| format!("+{e}")),
                blocked: contact.blocked,
            },
            None => Who::Other,
        },
        Some(Destination::Group(group)) if group.masterKey.len() == 32 => {
            use proto::group::group_attribute_blob::Content;
            let snapshot = group.snapshot.as_ref();
            let title = snapshot
                .and_then(|s| s.title.as_ref())
                .and_then(|t| match &t.content {
                    Some(Content::Title(title)) => Some(title.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let mut members: Vec<String> =
                snapshot.map(|s| s.members.iter().filter_map(|m| uuid_sid(&m.userId)).collect()).unwrap_or_default();
            members.sort();
            let ended = snapshot.is_some_and(|s| s.terminated);
            Who::Group { key: group.masterKey.clone(), title, members, blocked: group.blocked, ended }
        }
        // The phone's release notes chat is Desktop's Signal chat.
        Some(Destination::ReleaseNotes(_)) => Who::Contact {
            sid: signal_store::SIGNAL_ACI.into(),
            name: "Signal".into(),
            e164: None,
            blocked: false,
        },
        _ => Who::Other,
    }
}

/// A contact's or group's profile sharing (`Contact.profileSharing`,
/// `Group.whitelisted`) and a group's story send mode, as Desktop's backup
/// import reads them (`ts/services/backups/import`).
fn story_state(recipient: &proto::Recipient) -> Option<(ConversationId, bool, Option<signal_store::StorySendMode>)> {
    use proto::recipient::Destination;
    match &recipient.destination {
        Some(Destination::Contact(contact)) => {
            let sid = contact.aci.as_deref().and_then(uuid_sid)?;
            Some((ConversationId::direct(&sid), contact.profileSharing, None))
        }
        Some(Destination::Group(group)) if group.masterKey.len() == 32 => {
            let mode = signal_store::StorySendMode::from_wire(i64::from(group.storySendMode.value()));
            Some((ConversationId::group(&group.masterKey), group.whitelisted, Some(mode)))
        }
        _ => None,
    }
}

/// A contact with a name from the phone's address book (Desktop's import
/// keeps `systemGivenName`, `systemFamilyName` and `systemNickname`, which
/// `isInSystemContacts` reads): their service id.
fn system_contact(recipient: &proto::Recipient) -> Option<String> {
    use proto::recipient::Destination;
    let Some(Destination::Contact(contact)) = &recipient.destination else { return None };
    let named = [&contact.systemGivenName, &contact.systemFamilyName, &contact.systemNickname].iter().any(|n| !n.trim().is_empty());
    named.then(|| contact.aci.as_deref().and_then(uuid_sid)).flatten()
}

/// A message request answer in transferred history (Desktop's import of
/// `REPORTED_SPAM`, `BLOCKED`, `UNBLOCKED` and `MESSAGE_REQUEST_ACCEPTED`
/// simple updates as `message-request-response-event`s).
fn message_request_event(update: &proto::ChatUpdateMessage) -> Option<signal_store::MessageRequestEvent> {
    use proto::simple_chat_update::Type;
    use signal_store::MessageRequestEvent;
    let Some(proto::chat_update_message::Update::SimpleUpdate(simple)) = &update.update else { return None };
    match simple.type_.enum_value().ok()? {
        Type::REPORTED_SPAM => Some(MessageRequestEvent::Spam),
        Type::BLOCKED => Some(MessageRequestEvent::Block),
        Type::UNBLOCKED => Some(MessageRequestEvent::Unblock),
        Type::MESSAGE_REQUEST_ACCEPTED => Some(MessageRequestEvent::Accept),
        _ => None,
    }
}

/// The notices for a chat update in transferred history, as Desktop's
/// import reads them (`#fromChatItemUpdateMessage`,
/// `#fromSimpleUpdateMessage`, `#fromGroupUpdateMessage` in
/// `ts/services/backups/import.preload.ts`). `author` is the item's author
/// (`Some(None)`: us; `None`: unknown), `recipient` reads a recipient id
/// the same way. Kinds this app has no notice for (group creation, GV1
/// migration, member label access, payments, "joined Signal", ...) give
/// none, nor do updates missing who made them.
fn update_notices(
    update: &proto::ChatUpdateMessage,
    author: Option<Option<String>>,
    conversation: &ConversationId,
    me: &str,
    recipient: impl Fn(u64) -> Option<Option<String>>,
) -> Vec<signal_store::Notice> {
    use proto::chat_update_message::Update;
    use proto::simple_chat_update::Type;
    use signal_store::Notice;
    // The person a 1:1 update is about, else the author (Desktop's
    // `key_changed` and `changedId`).
    let subject = || conversation.as_direct().map(str::to_owned).or_else(|| author.clone().flatten());
    match &update.update {
        Some(Update::SimpleUpdate(simple)) => {
            let notice = match simple.type_.enum_value() {
                Ok(Type::IDENTITY_UPDATE) => subject().map(|who| Notice::SafetyNumberChanged { who }),
                // Imported without `local`, which Desktop shows as from
                // another device.
                Ok(Type::IDENTITY_VERIFIED) => author.clone().flatten().map(|who| Notice::Verified { who, verified: true, local: false }),
                Ok(Type::IDENTITY_DEFAULT) => author.clone().flatten().map(|who| Notice::Verified { who, verified: false, local: false }),
                Ok(Type::CHANGE_NUMBER) => subject().map(|who| Notice::NumberChanged { who }),
                Ok(Type::CHAT_SESSION_REFRESH) => Some(Notice::SessionRefreshed),
                Ok(Type::BAD_DECRYPT) => subject().map(|who| Notice::DeliveryIssue { who }),
                Ok(Type::UNSUPPORTED_PROTOCOL_MESSAGE) => author.clone().map(|who| Notice::UnsupportedMessage { who }),
                _ => None,
            };
            notice.into_iter().collect()
        }
        Some(Update::ProfileChange(change)) => subject()
            .map(|who| Notice::ProfileNameChanged { who, old_name: change.previousName.clone(), new_name: change.newName.clone() })
            .into_iter()
            .collect(),
        Some(Update::ExpirationTimerChange(change)) => author
            .map(|by| Notice::Timer { by, seconds: (change.expiresInMs / 1000) as u32 })
            .into_iter()
            .collect(),
        Some(Update::PinMessage(pin)) => match (author, recipient(pin.authorId)) {
            (Some(by), Some(target)) => vec![Notice::Pinned { by, author: target, sent_at: pin.targetSentTimestamp as i64 }],
            _ => Vec::new(),
        },
        Some(Update::GroupChange(change)) => change.updates.iter().filter_map(|u| group_update_notice(u, me)).collect(),
        _ => Vec::new(),
    }
}

/// One detail of a group change in transferred history as a notice:
/// Desktop's `#fromGroupUpdateMessage`, in this app's notices.
fn group_update_notice(update: &proto::group_change_chat_update::Update, me: &str) -> Option<signal_store::Notice> {
    use proto::group_change_chat_update::update::Update as U;
    use signal_store::{GroupNotice, Inviter, Notice};
    // A service id, `None` for us; the outer `None` when it's missing.
    let person = |bytes: &Option<Vec<u8>>| -> Option<Option<String>> {
        let sid = uuid_sid(bytes.as_deref()?)?;
        Some((sid != me).then_some(sid))
    };
    let required = |bytes: &Vec<u8>| -> Option<Option<String>> { person(&Some(bytes.clone())) };
    let inviter = |bytes: &Option<Vec<u8>>| match person(bytes) {
        Some(None) => Inviter::Us,
        Some(Some(id)) => Inviter::Person { id },
        None => Inviter::Unknown,
    };
    let admins_only = |level: proto::GroupV2AccessLevel| level == proto::GroupV2AccessLevel::ADMINISTRATOR;
    let group = |by: Option<String>, change: GroupNotice| Some(Notice::Group { by, change });
    match update.update.as_ref()? {
        U::GroupNameUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::Title { title: u.newGroupName.clone().unwrap_or_default() }),
        U::GroupAvatarUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::Avatar),
        U::GroupDescriptionUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::Description),
        U::GroupMembershipAccessLevelChangeUpdate(u) => group(
            person(&u.updaterAci)?,
            GroupNotice::MemberAccess { admins_only: admins_only(u.accessLevel.enum_value_or_default()) },
        ),
        U::GroupAttributesAccessLevelChangeUpdate(u) => group(
            person(&u.updaterAci)?,
            GroupNotice::InfoAccess { admins_only: admins_only(u.accessLevel.enum_value_or_default()) },
        ),
        U::GroupAnnouncementOnlyChangeUpdate(u) => {
            group(person(&u.updaterAci)?, GroupNotice::AnnouncementsOnly { on: u.isAnnouncementOnly })
        }
        U::GroupAdminStatusUpdate(u) => group(
            person(&u.updaterAci)?,
            GroupNotice::Admin { member: required(&u.memberAci)?, admin: u.wasAdminStatusGranted },
        ),
        U::GroupMemberLeftUpdate(u) => {
            let member = required(&u.aci)?;
            group(member.clone(), GroupNotice::Removed { member })
        }
        U::GroupMemberRemovedUpdate(u) => group(person(&u.removerAci)?, GroupNotice::Removed { member: required(&u.removedAci)? }),
        U::SelfInvitedToGroupUpdate(u) => group(person(&u.inviterAci)?, GroupNotice::Invited { member: None }),
        U::SelfInvitedOtherUserToGroupUpdate(u) => group(None, GroupNotice::Invited { member: required(&u.inviteeServiceId)? }),
        U::GroupUnknownInviteeUpdate(u) => group(person(&u.inviterAci)?, GroupNotice::InvitedMany { count: u.inviteeCount }),
        U::GroupInvitationAcceptedUpdate(u) => {
            let member = required(&u.newMemberAci)?;
            group(member.clone(), GroupNotice::AcceptedInvite { member, inviter: inviter(&u.inviterAci) })
        }
        U::GroupInvitationDeclinedUpdate(u) => {
            let member = person(&u.inviteeAci).flatten();
            group(member.clone(), GroupNotice::InviteRemoved { member, inviter: inviter(&u.inviterAci) })
        }
        U::GroupMemberJoinedUpdate(u) => {
            let member = required(&u.newMemberAci)?;
            group(member.clone(), GroupNotice::Added { member })
        }
        U::GroupMemberAddedUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::Added { member: required(&u.newMemberAci)? }),
        U::GroupSelfInvitationRevokedUpdate(u) => {
            group(person(&u.revokerAci)?, GroupNotice::InviteRemoved { member: None, inviter: Inviter::Unknown })
        }
        U::GroupInvitationRevokedUpdate(u) => {
            let by = person(&u.updaterAci)?;
            match u.invitees.as_slice() {
                [one] => {
                    let member = person(&one.inviteeAci).flatten();
                    group(by, GroupNotice::InviteRemoved { member, inviter: inviter(&one.inviterAci) })
                }
                many => {
                    // One inviter for them all, else unknown.
                    let first = many.first().map(|i| inviter(&i.inviterAci)).unwrap_or(Inviter::Unknown);
                    let same = many.iter().all(|i| inviter(&i.inviterAci) == first);
                    let inviter = if same { first } else { Inviter::Unknown };
                    group(by, GroupNotice::InvitesRemoved { count: many.len() as u32, inviter })
                }
            }
        }
        U::GroupJoinRequestUpdate(u) => {
            let member = required(&u.requestorAci)?;
            group(member.clone(), GroupNotice::Requested { member })
        }
        U::GroupJoinRequestApprovalUpdate(u) => {
            let member = required(&u.requestorAci)?;
            let by = person(&u.updaterAci)?;
            if u.wasApproved {
                group(by, GroupNotice::Approved { member })
            } else {
                group(by, GroupNotice::RequestRemoved { member })
            }
        }
        U::GroupJoinRequestCanceledUpdate(u) => {
            let member = required(&u.requestorAci)?;
            group(member.clone(), GroupNotice::RequestRemoved { member })
        }
        U::GroupSequenceOfRequestsAndCancelsUpdate(u) => {
            let member = required(&u.requestorAci)?;
            group(member.clone(), GroupNotice::RequestRemoved { member })
        }
        U::GroupInviteLinkResetUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::LinkReset),
        U::GroupInviteLinkEnabledUpdate(u) => {
            group(person(&u.updaterAci)?, GroupNotice::LinkOn { approval: u.linkRequiresAdminApproval })
        }
        U::GroupInviteLinkAdminApprovalUpdate(u) => {
            group(person(&u.updaterAci)?, GroupNotice::LinkApproval { on: u.linkRequiresAdminApproval })
        }
        U::GroupInviteLinkDisabledUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::LinkOff),
        U::GroupMemberJoinedByLinkUpdate(u) => {
            let member = required(&u.newMemberAci)?;
            group(member.clone(), GroupNotice::JoinedByLink { member })
        }
        U::GroupExpirationTimerUpdate(u) => Some(Notice::Timer { by: person(&u.updaterAci)?, seconds: (u.expiresInMs / 1000) as u32 }),
        U::GroupTerminateChangeUpdate(u) => group(person(&u.updaterAci)?, GroupNotice::Ended),
        _ => None,
    }
}

/// Imports a decrypted backup's chats and messages for account `me`.
/// `progress` gets the bytes of `plain` imported so far.
pub(crate) async fn import(db: &AppDb, plain: &[u8], me: &str, mut progress: impl FnMut(u64)) -> anyhow::Result<Imported> {
    let mut people: HashMap<u64, Who> = HashMap::new();
    let mut chats: HashMap<u64, ConversationId> = HashMap::new();
    let mut pinned: Vec<(u32, ConversationId)> = Vec::new();
    let mut states: Vec<ChatState> = Vec::new();
    // Whom we share our profile with, and groups' story send modes.
    let mut sharing: Vec<(ConversationId, bool)> = Vec::new();
    let mut modes: Vec<(ConversationId, signal_store::StorySendMode)> = Vec::new();
    // Who's in our address book (Desktop's `isInSystemContacts`).
    let mut system: Vec<String> = Vec::new();
    let mut imported = Imported::default();
    // Chat colors (Desktop's `#fromCustomChatColors` and `#fromChatStyle`):
    // custom colors by their backup id, then the default and each chat's.
    let mut colors = signal_store::ChatColors::default();
    let mut custom_by_id: HashMap<u64, (String, signal_store::CustomColor)> = HashMap::new();
    // Chat folders take their order from the backup's (Desktop's
    // `#chatFolderPositionCursor`).
    let mut folder_position = 0u32;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    let mut frames = delimited(plain);
    // First comes the BackupInfo.
    let info = proto::BackupInfo::parse_from_bytes(frames.next().context("empty backup")??)?;
    tracing::info!(version = info.version, "importing message history");
    for frame in frames {
        let frame = frame?;
        // Frames are slices of `plain`, so where this one ends is how far we are.
        progress((frame.as_ptr() as usize - plain.as_ptr() as usize + frame.len()) as u64);
        let frame = proto::Frame::parse_from_bytes(frame)?;
        match frame.item {
            Some(proto::frame::Item::Recipient(recipient)) => {
                if let Some((chat, on, mode)) = story_state(&recipient) {
                    sharing.push((chat.clone(), on));
                    modes.extend(mode.map(|mode| (chat, mode)));
                }
                system.extend(system_contact(&recipient));
                import_key_transparency(db, &recipient).await;
                people.insert(recipient.id, who(&recipient));
            }
            Some(proto::frame::Item::Chat(chat)) => {
                let Some(person) = people.get(&chat.recipientId) else { continue };
                let (id, update, blocked) = match person {
                    Who::Me => (
                        ConversationId::direct(me),
                        ConversationUpdate {
                            kind: Some(ConversationKind::NoteToSelf),
                            title: Some("Note to Self".into()),
                            ..Default::default()
                        },
                        false,
                    ),
                    Who::Contact { sid, name, e164, blocked } => (
                        ConversationId::direct(sid),
                        ConversationUpdate {
                            kind: Some(ConversationKind::Direct),
                            title: Some(name.clone()),
                            phone: e164.clone(),
                            color: Some(signal_store::avatar_color(sid)),
                            ..Default::default()
                        },
                        *blocked,
                    ),
                    Who::Group { key, title, members, blocked, ended } => (
                        ConversationId::group(key),
                        ConversationUpdate {
                            kind: Some(ConversationKind::Group),
                            title: Some(title.clone()).filter(|t| !t.is_empty()),
                            members: Some(members.clone()),
                            // Never un-ends one ended while importing.
                            ended: ended.then_some(true),
                            ..Default::default()
                        },
                        *blocked,
                    ),
                    Who::Other => continue,
                };
                db.upsert_conversation(&id, update).await?;
                let muted_until = chat.muteUntilMs.map_or(0, |m| i64::try_from(m).unwrap_or(i64::MAX));
                // Applied after the messages: importing one would unarchive.
                states.push(ChatState { id: id.clone(), archived: chat.archived, muted_until, marked_unread: chat.markedUnread, blocked });
                if let Some(ms) = chat.expirationTimerMs.filter(|ms| *ms > 0) {
                    db.set_expire_timer(&id, (ms / 1000) as u32, chat.expireTimerVersion.max(1)).await?;
                }
                if let Some(order) = chat.pinnedOrder {
                    pinned.push((order, id.clone()));
                }
                if let Some(color) = chat.style.as_ref().and_then(|style| chat_color(style, &custom_by_id)) {
                    colors.chats.insert(id.0.clone(), color);
                }
                chats.insert(chat.id, id);
                imported.chats += 1;
            }
            Some(proto::frame::Item::Account(account)) => {
                imported.settings = Some(account_settings(&account));
                if let Some(settings) = account.accountSettings.as_ref() {
                    for custom in &settings.customChatColors {
                        let Some(color) = custom.color.as_ref().and_then(custom_color) else { continue };
                        let id = uuid::Uuid::new_v4().to_string();
                        colors.custom.colors.push((id.clone(), color));
                        custom_by_id.insert(custom.id, (id, color));
                    }
                    if let Some(color) = settings.defaultChatStyle.as_ref().and_then(|style| chat_color(style, &custom_by_id)) {
                        colors.default = color;
                    }
                }
                // Desktop: `hasKeyTransparencyDisabled` unless the account allows it.
                let allowed = account.accountSettings.as_ref().is_some_and(|s| s.allowAutomaticKeyVerification);
                let value = if allowed { "false" } else { "true" };
                if let Err(error) = db.set_setting(crate::key_transparency::SETTING_DISABLED, value).await {
                    tracing::warn!(%error, "failed to save automatic key verification");
                }
                // Desktop's import also takes who can find us by number
                // and the app's theme.
                let hidden = account.accountSettings.as_ref().is_some_and(|s| s.notDiscoverableByPhoneNumber);
                let discoverable = if hidden { "false" } else { "true" };
                for (key, value) in [(crate::SETTING_PHONE_NUMBER_DISCOVERABLE, discoverable), (crate::SETTING_THEME, theme(&account))] {
                    if let Err(error) = db.set_setting(key, value).await {
                        tracing::warn!(%error, key, "failed to save an account setting from the backup");
                    }
                }
            }
            Some(proto::frame::Item::ChatFolder(folder)) => {
                if let Some(folder) = chat_folder(&folder, &people, me, folder_position) {
                    folder_position += 1;
                    db.edit_chat_folders(signal_store::ChatFolderEdit::Merge(folder)).await?;
                }
            }
            Some(proto::frame::Item::StickerPack(pack)) => {
                imported.sticker_packs.push((pack.packId.clone(), pack.packKey.clone()));
            }
            Some(proto::frame::Item::ChatItem(item)) => {
                imported.messages +=
                    usize::from(import_item(db, &item, &people, &chats, me, now, &mut imported).await?);
            }
            _ => {}
        }
    }
    db.apply_chat_states(&states).await?;
    if colors != signal_store::ChatColors::default() {
        db.edit_chat_colors(signal_store::ChatColorEdit::Import(colors)).await?;
    }
    db.apply_profile_sharing(&sharing).await?;
    db.add_system_contacts(&system).await?;
    db.set_story_send_modes(&modes).await?;
    if !pinned.is_empty() {
        pinned.sort_by_key(|(order, _)| *order);
        db.replace_pinned(&pinned.into_iter().map(|(_, id)| id).collect::<Vec<_>>()).await?;
    }
    Ok(imported)
}

/// A backup's chat folder (Desktop's `#fromChatFolder`): one without a
/// UUID or of a type it doesn't know is dropped; recipients become chats.
fn chat_folder(folder: &proto::ChatFolder, people: &HashMap<u64, Who>, me: &str, position: u32) -> Option<signal_store::ChatFolder> {
    use proto::chat_folder::FolderType;
    let id = uuid::Uuid::from_slice(&folder.id).ok()?;
    let folder_type = match folder.folderType.enum_value() {
        Ok(FolderType::ALL) => signal_store::ChatFolderType::All,
        Ok(FolderType::CUSTOM) => signal_store::ChatFolderType::Custom,
        _ => return None,
    };
    let chat = |recipient: &u64| match people.get(recipient)? {
        Who::Me => Some(ConversationId::direct(me)),
        Who::Contact { sid, .. } => Some(ConversationId::direct(sid)),
        Who::Group { key, .. } => Some(ConversationId::group(key)),
        Who::Other => None,
    };
    Some(signal_store::ChatFolder {
        id: id.to_string(),
        folder_type,
        name: folder.name.clone(),
        position,
        show_only_unread: folder.showOnlyUnread,
        show_muted_chats: folder.showMutedChats,
        include_all_individual_chats: folder.includeAllIndividualChats,
        include_all_group_chats: folder.includeAllGroupChats,
        included: folder.includedRecipientIds.iter().filter_map(chat).collect(),
        excluded: folder.excludedRecipientIds.iter().filter_map(chat).collect(),
        deleted_at: 0,
    })
}

/// Desktop's `rgbIntToDesktopHSL`: a backup's `0xAARRGGBB` as Desktop's
/// hue (degrees), saturation (percent) and lightness (0 to 1), as
/// `ts/util/rgbToHSL.std.ts` computes them.
fn desktop_hsl(argb: u32) -> signal_store::HueSaturation {
    let [_, r, g, b] = argb.to_be_bytes().map(|c| f64::from(c) / 255.);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let c = max - min;
    let l = max - c / 2.;
    let h = if c == 0. {
        0.
    } else if max == r {
        60. * (((g - b) / c + 6.) % 6.)
    } else if max == g {
        60. * ((b - r) / c + 2.)
    } else {
        60. * ((r - g) / c + 4.)
    };
    let s = if l == 0. || l == 1. { 0. } else { (max - l) / l.min(1. - l) };
    signal_store::HueSaturation { hue: h as f32, saturation: (s * 100.) as f32, lightness: Some(l as f32) }
}

/// A backup's custom color (Desktop's `#fromCustomChatColors`): solid, or a
/// gradient from its first color to its last, its angle turned to
/// Desktop's (`360 - angle - 90`).
fn custom_color(color: &proto::chat_style::custom_chat_color::Color) -> Option<signal_store::CustomColor> {
    use proto::chat_style::custom_chat_color::Color;
    match color {
        Color::Solid(argb) => Some(signal_store::CustomColor { start: desktop_hsl(*argb), end: None, deg: None }),
        Color::Gradient(gradient) => {
            let (start, end) = (gradient.colors.first()?, gradient.colors.last()?);
            let deg = (360 - i64::from(gradient.angle) - 90).rem_euclid(360) as f32;
            Some(signal_store::CustomColor { start: desktop_hsl(*start), end: Some(desktop_hsl(*end)), deg: Some(deg) })
        }
        _ => None,
    }
}

/// A chat style's bubble color (Desktop's `#fromChatStyle`): a preset, a
/// custom color by its backup id, or none for the automatic color (no
/// wallpaper is imported, as Desktop imports none outside its tests).
fn chat_color(
    style: &proto::ChatStyle,
    custom: &HashMap<u64, (String, signal_store::CustomColor)>,
) -> Option<signal_store::ChatColor> {
    use proto::chat_style::BubbleColor;
    match style.bubbleColor.as_ref()? {
        BubbleColor::AutoBubbleColor(_) => None,
        BubbleColor::BubbleColorPreset(preset) => {
            let index = usize::try_from(preset.value() - 1).ok().filter(|i| *i < signal_store::CHAT_COLOR_PRESETS.len()).unwrap_or(0);
            Some(signal_store::ChatColor::preset(signal_store::CHAT_COLOR_PRESETS[index]))
        }
        BubbleColor::CustomColorId(id) => custom.get(id).map(|(id, value)| signal_store::ChatColor::Custom { id: id.clone(), value: *value }),
        _ => None,
    }
}

/// One chat item; `Ok(false)` for kinds that aren't imported (updates
/// `update_notices` has no notice for, calls, payments, …) or that have
/// already disappeared.
async fn import_item(
    db: &AppDb,
    item: &proto::ChatItem,
    people: &HashMap<u64, Who>,
    chats: &HashMap<u64, ConversationId>,
    me: &str,
    now: i64,
    imported: &mut Imported,
) -> anyhow::Result<bool> {
    use proto::chat_item::{DirectionalDetails, Item};
    let Some(conversation) = chats.get(&item.chatId) else { return Ok(false) };
    let expires_in = item.expiresInMs.map_or(0, |ms| (ms / 1000) as u32);
    if let (Some(start), true) = (item.expireStartDate, expires_in > 0)
        && start as i64 + i64::from(expires_in) * 1000 <= now
    {
        return Ok(false);
    }
    let person = |id: u64| -> (Option<String>, Option<String>) {
        match people.get(&id) {
            Some(Who::Contact { sid, name, .. }) => (Some(sid.clone()), Some(name.clone())),
            _ => (None, None),
        }
    };
    // A poll's end: Desktop's "… ended the poll" notice (an update, with
    // no direction).
    if let Some(Item::UpdateMessage(update)) = &item.item
        && let Some(proto::chat_update_message::Update::PollTerminate(end)) = &update.update
    {
        let by = match people.get(&item.authorId) {
            Some(Who::Me) => None,
            Some(Who::Contact { sid, .. }) => Some(sid.clone()),
            _ => return Ok(false),
        };
        let notice = signal_store::Notice::PollEnded { by, question: end.question.clone(), sent_at: end.targetSentTimestamp as i64 };
        return Ok(db.insert_notice(conversation, item.dateSent as i64, &notice).await?.is_some());
    }
    if let Some(Item::UpdateMessage(update)) = &item.item
        && let Some(event) = message_request_event(update)
    {
        let group = conversation.as_group().is_some();
        let who = conversation.as_direct().map(str::to_owned);
        let notice = signal_store::Notice::MessageRequest { event, group, who };
        return Ok(db.insert_notice(conversation, item.dateSent as i64, &notice).await?.is_some());
    }
    // The other updates this app shows, a millisecond apart as live group
    // changes are (each of a group change's details is a notice here).
    if let Some(Item::UpdateMessage(update)) = &item.item {
        let author = match people.get(&item.authorId) {
            Some(Who::Me) => Some(None),
            Some(Who::Contact { sid, .. }) => Some(Some(sid.clone())),
            _ => None,
        };
        let mut inserted = false;
        let recipient = |id: u64| match people.get(&id) {
            Some(Who::Me) => Some(None),
            Some(Who::Contact { sid, .. }) => Some(Some(sid.clone())),
            _ => None,
        };
        for (ix, notice) in update_notices(update, author, conversation, me, recipient).into_iter().enumerate() {
            inserted |= db.insert_notice(conversation, item.dateSent as i64 + ix as i64, &notice).await?.is_some();
        }
        return Ok(inserted);
    }
    let (author, author_name) = person(item.authorId);
    let (direction, received_at, status, read) = match &item.directionalDetails {
        Some(DirectionalDetails::Incoming(details)) => {
            (Direction::Incoming, details.dateReceived as i64, DeliveryStatus::Delivered, details.read)
        }
        Some(DirectionalDetails::Outgoing(details)) => {
            (Direction::Outgoing, details.dateReceived as i64, outgoing_status(&details.sendStatus), true)
        }
        _ => return Ok(false),
    };
    // Ours have no author; theirs must have one.
    let (author, author_name) = match direction {
        Direction::Outgoing => (None, None),
        Direction::Incoming if author.is_some() => (author, author_name),
        Direction::Incoming => return Ok(false),
    };

    let mut poll = None;
    let (mut message, reactions, mentions, deleted) = match &item.item {
        Some(Item::StandardMessage(standard)) => {
            let text = standard.text.as_ref();
            let body = text.map(|t| t.body.clone()).unwrap_or_default();
            let mut ranges: Vec<(u32, String)> = text
                .map(|t| {
                    t.bodyRanges
                        .iter()
                        .filter_map(|r| match &r.associatedValue {
                            Some(proto::body_range::AssociatedValue::MentionAci(aci)) => Some((r.start, uuid_sid(aci)?)),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            ranges.sort_by_key(|(start, _)| *start);
            let mentions: Vec<String> = ranges.into_iter().map(|(_, sid)| sid).collect();
            let attachments = standard
                .attachments
                .iter()
                .filter_map(|a| {
                    let mut meta = attachment(a.pointer.as_ref()?);
                    meta.voice = a.flag.enum_value().ok() == Some(proto::message_attachment::Flag::VOICE_MESSAGE);
                    Some(meta)
                })
                .collect();
            let link_preview = standard.linkPreview.first().map(|p| LinkPreview {
                url: p.url.clone(),
                title: p.title.clone(),
                description: p.description.clone(),
                date: p.date.map(|d| d as i64),
                image: None,
            });
            let quote = standard.quote.as_ref().and_then(|q| {
                let (author, name) = person(q.authorId);
                Some(Quote {
                    author_name: name.unwrap_or_else(|| "You".into()),
                    author,
                    sent_at: q.targetSentTimestamp? as i64,
                    text: q.text.as_ref().map(|t| t.body.clone()).unwrap_or_default(),
                    thumbnail: None,
                    // A quote of a view-once message says only that
                    // (Desktop's import: `isViewOnce` from its type).
                    attachment: if q.type_.enum_value_or_default() == proto::quote::Type::VIEW_ONCE {
                        Some(signal_store::QuotedAttachment::view_once())
                    } else {
                        q.attachments.first().map(|a| {
                            use proto::message_attachment::Flag;
                            let flag = a.thumbnail.as_ref().map(|t| t.flag.enum_value_or_default());
                            signal_store::QuotedAttachment {
                                content_type: a.contentType.clone().unwrap_or_default(),
                                file_name: a.fileName.clone(),
                                voice: flag == Some(Flag::VOICE_MESSAGE),
                                gif: flag == Some(Flag::GIF),
                                view_once: false,
                            }
                        })
                    },
                    story: false,
                })
            });
            let message = NewMessage {
                conversation_id: conversation.clone(),
                sent_at: item.dateSent as i64,
                received_at,
                direction,
                author: author.clone(),
                author_name: author_name.clone(),
                body,
                status,
                quote,
                attachments,
                link_preview,
                expires_in,
            };
            (message, standard.reactions.clone(), mentions, false)
        }
        Some(Item::StickerMessage(sticker)) => {
            let mut meta = sticker.sticker.data.as_ref().map(attachment).unwrap_or_else(|| attachment(&Default::default()));
            meta.sticker = true;
            let message = NewMessage {
                conversation_id: conversation.clone(),
                sent_at: item.dateSent as i64,
                received_at,
                direction,
                author: author.clone(),
                author_name: author_name.clone(),
                body: String::new(),
                status,
                quote: None,
                attachments: vec![meta],
                link_preview: None,
                expires_in,
            };
            (message, sticker.reactions.clone(), Vec::new(), false)
        }
        Some(Item::Poll(exported)) => {
            let options: Vec<String> = exported.options.iter().map(|o| o.option.clone()).collect();
            if !signal_store::valid_poll(&exported.question, &options) {
                return Ok(false);
            }
            // The backup lists voters per option; we keep each voter's
            // choices together. When they voted isn't kept.
            let mut votes: Vec<signal_store::PollVote> = Vec::new();
            for (index, option) in exported.options.iter().enumerate() {
                for voter in &option.votes {
                    let author = match people.get(&voter.voterId) {
                        Some(Who::Me) => None,
                        Some(Who::Contact { sid, .. }) => Some(sid.clone()),
                        _ => continue,
                    };
                    match votes.iter_mut().find(|v| v.author == author) {
                        Some(vote) => {
                            vote.options.push(index as u32);
                            vote.vote_count = vote.vote_count.max(voter.voteCount);
                        }
                        None => votes.push(signal_store::PollVote {
                            author,
                            options: vec![index as u32],
                            vote_count: voter.voteCount,
                            sent_at: 0,
                            ..Default::default()
                        }),
                    }
                }
            }
            let mut imported = signal_store::Poll::new(exported.question.clone(), options, exported.allowMultiple);
            imported.votes = votes;
            imported.ended = exported.hasEnded;
            // Ours reached whoever the backup has it as sent to; only they
            // may vote (Desktop imports each recipient's send status).
            if let Some(DirectionalDetails::Outgoing(details)) = &item.directionalDetails {
                imported.record_reached(reached(&details.sendStatus, people));
            }
            poll = Some(imported);
            let message = NewMessage {
                conversation_id: conversation.clone(),
                sent_at: item.dateSent as i64,
                received_at,
                direction,
                author: author.clone(),
                author_name: author_name.clone(),
                body: exported.question.clone(),
                status,
                quote: None,
                attachments: Vec::new(),
                link_preview: None,
                expires_in,
            };
            (message, exported.reactions.clone(), Vec::new(), false)
        }
        Some(Item::RemoteDeletedMessage(_)) => {
            let message = NewMessage {
                conversation_id: conversation.clone(),
                sent_at: item.dateSent as i64,
                received_at,
                direction,
                author: author.clone(),
                author_name: author_name.clone(),
                body: String::new(),
                status,
                quote: None,
                attachments: Vec::new(),
                link_preview: None,
                expires_in,
            };
            (message, Vec::new(), Vec::new(), true)
        }
        _ => return Ok(false),
    };

    // Its formatting, over the body as exported.
    let styles = match &item.item {
        Some(Item::StandardMessage(standard)) => standard.text.as_ref().map(|t| styles(&t.bodyRanges)).unwrap_or_default(),
        _ => Vec::new(),
    };
    // "@Name" in place of each mention placeholder.
    let names: Vec<String> = mentions
        .iter()
        .map(|sid| {
            if sid == me {
                return "You".to_owned();
            }
            people
                .values()
                .find_map(|p| match p {
                    Who::Contact { sid: s, name, .. } if s == sid => Some(name.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Unknown".into())
        })
        .collect();
    // The whole text, when it's an attachment to download; Desktop keeps
    // the body and its ranges for it (`ts/services/backups/import.preload.ts`).
    let long_text = match &item.item {
        Some(Item::StandardMessage(standard)) => standard.longText.as_ref().and_then(|pointer| {
            let locator = pointer.locatorInfo.as_ref()?;
            Some(signal_store::LongText {
                remote: remote(locator)?,
                size: u64::from(locator.size),
                mentions: mentions.clone(),
                styles: styles.clone(),
                state: signal_store::LongTextState::Downloading,
                attempts: 0,
                retry_at: None,
            })
        }),
        _ => None,
    };
    let (body, spans, style_spans) = crate::mapping::render_body(&message.body, &mentions, &names, &styles);
    message.body = body;
    // An edited message is the latest version, dated by the latest edit; it
    // goes by its original (oldest) date, and its edits' dates name it too.
    let mut edits: Vec<i64> = item.revisions.iter().map(|r| r.dateSent as i64).collect();
    if !edits.is_empty() {
        edits.push(message.sent_at);
        edits.sort_unstable();
        message.sent_at = edits.remove(0);
    }
    let id = db.insert_message(&message).await?;
    // Who ours went to and how far it got with each (Note to Self has none).
    if let Some(DirectionalDetails::Outgoing(details)) = &item.directionalDetails {
        let states = send_states(&details.sendStatus, people);
        if !states.is_empty() {
            db.import_send_states(id, &states).await?;
        }
    }
    for (index, attachment) in message.attachments.iter().enumerate() {
        let Some(remote) = &attachment.remote else { continue };
        let thumbnail = remote.plaintext_hash.is_some() && crate::backup_media::can_have_thumbnail(&attachment.content_type);
        if crate::actor::downloads_whole(attachment) || thumbnail {
            imported.downloads.push((id, index));
        }
    }
    for at in &edits {
        db.edit_message(id, &message.body, *at).await?;
    }
    // Its earlier versions, each with its own text (the backup lists them
    // oldest to newest), for the edit history.
    if !item.revisions.is_empty() {
        let history: Vec<signal_store::EditRevision> = item
            .revisions
            .iter()
            .map(|revision| {
                let text = match &revision.item {
                    Some(Item::StandardMessage(standard)) => standard.text.as_ref(),
                    _ => None,
                };
                let (body, mentions, styles) = shown_text(text, people, me);
                signal_store::EditRevision { sent_at: revision.dateSent as i64, body, mentions, styles }
            })
            .collect();
        db.set_edit_history(id, &history).await?;
    }
    if !spans.is_empty() {
        db.set_mentions(id, &spans).await?;
    }
    if !style_spans.is_empty() {
        db.set_styles(id, &style_spans).await?;
    }
    if let Some(poll) = &poll {
        db.set_poll(id, poll).await?;
    }
    if deleted {
        db.mark_deleted(id).await?;
    } else if let Some(long_text) = &long_text {
        db.set_long_text(id, Some(long_text)).await?;
        imported.long_texts.push(id);
    }
    for reaction in reactions {
        let (author, _) = person(reaction.authorId);
        db.set_reaction(id, Reaction { emoji: reaction.emoji, author, sent_at: reaction.sentTimestamp as i64 }).await?;
    }
    if read && let (Direction::Incoming, Some(author)) = (direction, &message.author) {
        db.mark_message_read(author, message.sent_at).await?;
    }
    if let Some(pin) = item.pinDetails.as_ref() {
        use proto::chat_item::pin_details::PinExpiry;
        let expires = match pin.pinExpiry {
            Some(PinExpiry::PinExpiresAtTimestamp(at)) => Some(at as i64),
            _ => None,
        };
        db.pin_message(conversation, id, pin.pinnedAtTimestamp as i64, expires, crate::remote_config().pinned_message_limit()).await?;
    }
    Ok(true)
}

/// The backup's privacy settings, as Desktop's import
/// (`ts/services/backups/import.preload.ts`) takes them: each on only when
/// the backup says so.
fn account_settings(account: &proto::AccountData) -> crate::AccountSettings {
    use crate::AccountSetting as S;
    use proto::account_data::PhoneNumberSharingMode;
    let settings = account.accountSettings.as_ref();
    let on = |get: fn(&proto::account_data::AccountSettings) -> bool| settings.is_some_and(get);
    let flags = [
        (S::ReadReceipts, on(|s| s.readReceipts)),
        (S::TypingIndicators, on(|s| s.typingIndicators)),
        (S::LinkPreviews, on(|s| s.linkPreviews)),
        (S::SealedSenderIndicators, on(|s| s.sealedSenderIndicators)),
        (S::PreferContactAvatars, on(|s| s.preferContactAvatars)),
        (S::KeepMutedChatsArchived, on(|s| s.keepMutedChatsArchived)),
    ];
    // Desktop sets the timer only when there is one, and takes anything
    // but EVERYBODY as nobody.
    let timer = settings.map_or(0, |s| s.universalExpireTimerSeconds);
    let everybody = settings.is_some_and(|s| s.phoneNumberSharingMode.enum_value_or_default() == PhoneNumberSharingMode::EVERYBODY);
    crate::AccountSettings {
        flags: flags.into_iter().collect(),
        universal_expire_timer: (timer > 0).then_some(timer),
        phone_number_sharing: Some(everybody),
    }
}

/// The backup's app theme, as Desktop's `toThemeSetting`: light, dark, or
/// the system's.
fn theme(account: &proto::AccountData) -> &'static str {
    use proto::account_data::AppTheme;
    match account.accountSettings.as_ref().map(|s| s.appTheme.enum_value_or_default()) {
        Some(AppTheme::LIGHT) => "light",
        Some(AppTheme::DARK) => "dark",
        _ => "system",
    }
}

/// Each recipient's send state from an outgoing message's statuses, as
/// Desktop's import reads them (`fromSendStatus`): pending when unset, a
/// key mismatch kept as one (message details offer the safety number).
/// Desktop keeps a pending one pending; see below.
fn send_states(statuses: &[proto::SendStatus], people: &HashMap<u64, Who>) -> Vec<signal_store::SendState> {
    use proto::send_status::DeliveryStatus as S;
    use signal_store::SendStatus;
    statuses
        .iter()
        .filter_map(|s| {
            let Some(Who::Contact { sid, .. }) = people.get(&s.recipientId) else { return None };
            let (status, error) = match &s.deliveryStatus {
                Some(S::Sent(_)) => (SendStatus::Sent, None),
                Some(S::Delivered(_)) => (SendStatus::Delivered, None),
                Some(S::Read(_)) => (SendStatus::Read, None),
                Some(S::Viewed(_)) => (SendStatus::Viewed, None),
                Some(S::Skipped(_)) => (SendStatus::Skipped, None),
                Some(S::Failed(failed)) => {
                    let identity = failed.reason.enum_value_or_default() == proto::send_status::failed::FailureReason::IDENTITY_KEY_MISMATCH;
                    let error = if identity { signal_store::SEND_ERROR_IDENTITY } else { "failed on your phone" };
                    (SendStatus::Failed, Some(error.to_owned()))
                }
                // Still pending on the phone: it wasn't sent, and as before
                // these were kept it's failed here, so Resend can send it.
                _ => (SendStatus::Failed, Some("not sent from your phone".to_owned())),
            };
            Some(signal_store::SendState { recipient: sid.clone(), status, updated_at: Some(s.timestamp as i64).filter(|t| *t > 0), error })
        })
        .collect()
}

/// Who an outgoing message reached: the recipients whose status is sent or
/// beyond, or skipped (Desktop's `isSent`).
fn reached(statuses: &[proto::SendStatus], people: &HashMap<u64, Who>) -> Vec<String> {
    use proto::send_status::DeliveryStatus as S;
    statuses
        .iter()
        .filter(|s| matches!(s.deliveryStatus, Some(S::Sent(_) | S::Delivered(_) | S::Read(_) | S::Viewed(_) | S::Skipped(_))))
        .filter_map(|s| match people.get(&s.recipientId) {
            Some(Who::Contact { sid, .. }) => Some(sid.clone()),
            _ => None,
        })
        .collect()
}

fn outgoing_status(statuses: &[proto::SendStatus]) -> DeliveryStatus {
    use proto::send_status::DeliveryStatus as S;
    let rank = |s: &proto::SendStatus| match &s.deliveryStatus {
        Some(S::Read(_) | S::Viewed(_)) => 3,
        Some(S::Delivered(_)) => 2,
        Some(S::Sent(_) | S::Skipped(_)) => 1,
        Some(S::Failed(_)) => -1,
        Some(S::Pending(_)) | None => 0,
        Some(_) => 0,
    };
    // Note to Self has no statuses: sent. One still pending on the phone
    // wasn't sent: failed here, so Retry Send can send it.
    match statuses.iter().map(rank).max() {
        None | Some(1) => DeliveryStatus::Sent,
        Some(0) => DeliveryStatus::Failed,
        Some(2) => DeliveryStatus::Delivered,
        Some(3) => DeliveryStatus::Read,
        Some(_) => DeliveryStatus::Failed,
    }
}

/// An attachment's details (its file isn't transferred yet).
fn attachment(pointer: &proto::FilePointer) -> AttachmentMeta {
    let locator = pointer.locatorInfo.as_ref();
    AttachmentMeta {
        content_type: pointer.contentType.clone().unwrap_or_else(|| "application/octet-stream".into()),
        file_name: pointer.fileName.clone(),
        size: locator.map_or(0, |l| u64::from(l.size)),
        width: pointer.width,
        height: pointer.height,
        local_path: None,
        sticker: false,
        gif: false,
        voice: false,
        remote: locator.and_then(remote),
        thumbnail_path: None,
    }
}

/// Where a transferred attachment can be downloaded, as Desktop reads a
/// locator (`ts/services/backups/util/filePointers.preload.ts`): the transit
/// tier, which the phone fills for link and sync (and which expires), when
/// it has a transit key; and the backup (media) tier, when the key and
/// plaintext hash name a file there (`backup_media.rs`). Neither: `None`.
fn remote(locator: &proto::file_pointer::LocatorInfo) -> Option<RemoteAttachment> {
    use proto::file_pointer::locator_info::IntegrityCheck;
    if locator.key.len() != 64 {
        return None;
    }
    let (digest, plaintext_hash) = match &locator.integrityCheck {
        Some(IntegrityCheck::EncryptedDigest(d)) if !d.is_empty() => (Some(hex::encode(d)), None),
        Some(IntegrityCheck::PlaintextHash(h)) if !h.is_empty() => (None, Some(hex::encode(h))),
        _ => return None,
    };
    let cdn_key = locator.transitCdnKey.clone().filter(|k| !k.is_empty());
    // Desktop keeps the media tier's CDN only alongside a media name.
    let media_tier_cdn = plaintext_hash.as_ref().and(locator.mediaTierCdnNumber);
    if cdn_key.is_none() && plaintext_hash.is_none() {
        return None;
    }
    Some(RemoteAttachment {
        cdn: locator.transitCdnNumber.unwrap_or(0),
        cdn_key,
        key: hex::encode(&locator.key),
        digest,
        plaintext_hash,
        media_tier_cdn,
    })
}

/// A backup's text styles (as on the wire: UTF-16, over the body with its
/// mention placeholders).
/// A message's text as shown: "@Name" for each mention (in body order),
/// with where the mentions and formatting land.
fn shown_text(
    text: Option<&proto::Text>,
    people: &HashMap<u64, Who>,
    me: &str,
) -> (String, Vec<signal_store::MentionSpan>, Vec<signal_store::StyleSpan>) {
    let Some(text) = text else { return Default::default() };
    let mut ranges: Vec<(u32, String)> = text
        .bodyRanges
        .iter()
        .filter_map(|r| match &r.associatedValue {
            Some(proto::body_range::AssociatedValue::MentionAci(aci)) => Some((r.start, uuid_sid(aci)?)),
            _ => None,
        })
        .collect();
    ranges.sort_by_key(|(start, _)| *start);
    let mentions: Vec<String> = ranges.into_iter().map(|(_, sid)| sid).collect();
    let names: Vec<String> = mentions
        .iter()
        .map(|sid| {
            if sid == me {
                return "You".to_owned();
            }
            people
                .values()
                .find_map(|p| match p {
                    Who::Contact { sid: s, name, .. } if s == sid => Some(name.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Unknown".into())
        })
        .collect();
    crate::mapping::render_body(&text.body, &mentions, &names, &styles(&text.bodyRanges))
}

fn styles(ranges: &[proto::BodyRange]) -> Vec<crate::mapping::WireStyle> {
    use proto::body_range::{AssociatedValue, Style};
    use signal_store::TextStyle;
    ranges
        .iter()
        .filter_map(|r| {
            let Some(AssociatedValue::Style(style)) = &r.associatedValue else { return None };
            let style = match style.enum_value().ok()? {
                Style::BOLD => TextStyle::Bold,
                Style::ITALIC => TextStyle::Italic,
                Style::SPOILER => TextStyle::Spoiler,
                Style::STRIKETHROUGH => TextStyle::Strikethrough,
                Style::MONOSPACE => TextStyle::Monospace,
                Style::NONE => return None,
            };
            (r.length > 0).then_some(crate::mapping::WireStyle { start: r.start, len: r.length, style })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACI: Aci = Aci::from_uuid_bytes([0x11; 16]);

    #[test]
    fn decrypts_libsignals_encrypted_sample() {
        use libsignal_account_keys::BackupKey;
        let pool = "m".repeat(64).parse().unwrap();
        let backup_key: BackupKey = BackupKey::derive_from_account_entropy_pool(&pool);
        let data = include_bytes!("../tests/res/backup/legacy-account.binproto.encrypted").to_vec();
        let plain = decrypt(data.clone(), backup_key.0, ACI).unwrap();
        let mut frames = delimited(&plain);
        let info = proto::BackupInfo::parse_from_bytes(frames.next().unwrap().unwrap()).unwrap();
        assert!(info.version >= 1);
        assert!(frames.all(|f| f.is_ok_and(|f| proto::Frame::parse_from_bytes(f).is_ok())));
        // The wrong key fails the MAC.
        assert!(decrypt(data, [7; 32], ACI).is_err());
    }

    #[tokio::test]
    async fn imports_libsignals_canonical_backup() {
        let db = AppDb::open_in_memory().await.unwrap();
        let plain = include_bytes!("../tests/res/backup/canonical-backup.binproto");
        let me = ACI.service_id_string();
        let imported = import(&db, plain, &me, |_| {}).await.unwrap();
        let chats = db.conversations().await.unwrap();
        assert_eq!(chats.len(), imported.chats, "{chats:#?}");
    }

    /// A small backup: you, Alice, a group; a chat with each; messages.
    fn sample_backup() -> Vec<u8> {
        use proto::chat_item::{DirectionalDetails, IncomingMessageDetails, Item, OutgoingMessageDetails};
        let alice = [0xa1; 16];
        let mut out = Vec::new();
        fn put<M: protobuf::Message>(out: &mut Vec<u8>, message: &M) {
            let bytes = message.write_to_bytes().unwrap();
            let mut len = bytes.len();
            while len >= 0x80 {
                out.push((len as u8) | 0x80);
                len >>= 7;
            }
            out.push(len as u8);
            out.extend(bytes);
        }
        put(&mut out, &proto::BackupInfo { version: 1, ..Default::default() });
        let mut push = |frame: proto::Frame| put(&mut out, &frame);
        // The account: read receipts and typing indicators on.
        let mut account = proto::AccountData::new();
        let mut settings = proto::account_data::AccountSettings::new();
        settings.readReceipts = true;
        settings.typingIndicators = true;
        settings.keepMutedChatsArchived = true;
        settings.universalExpireTimerSeconds = 604_800;
        settings.phoneNumberSharingMode = proto::account_data::PhoneNumberSharingMode::EVERYBODY.into();
        settings.notDiscoverableByPhoneNumber = true;
        settings.appTheme = proto::account_data::AppTheme::DARK.into();
        account.accountSettings = Some(settings).into();
        push(proto::Frame { item: Some(proto::frame::Item::Account(account)), ..Default::default() });
        let recipient = |id, destination| {
            let mut r = proto::Recipient::new();
            r.id = id;
            r.destination = Some(destination);
            proto::Frame { item: Some(proto::frame::Item::Recipient(r)), ..Default::default() }
        };
        use proto::recipient::Destination;
        push(recipient(1, Destination::Self_(Default::default())));
        let mut contact = proto::Contact::new();
        contact.aci = Some(alice.to_vec());
        contact.profileGivenName = Some("Alice".into());
        contact.profileFamilyName = Some("Liddell".into());
        contact.e164 = Some(15550100);
        contact.profileSharing = true;
        push(recipient(2, Destination::Contact(contact)));
        let mut group = proto::Group::new();
        group.masterKey = vec![9; 32];
        group.storySendMode = proto::group::StorySendMode::ENABLED.into();
        let mut snapshot = proto::group::GroupSnapshot::new();
        let mut title = proto::group::GroupAttributeBlob::new();
        title.content = Some(proto::group::group_attribute_blob::Content::Title("Hikers".into()));
        snapshot.title = Some(title).into();
        let mut member = proto::group::Member::new();
        member.userId = alice.to_vec();
        snapshot.members.push(member);
        // An admin ended it.
        snapshot.terminated = true;
        group.snapshot = Some(snapshot).into();
        push(recipient(3, Destination::Group(group)));
        push(recipient(4, Destination::ReleaseNotes(Default::default())));
        let chat = |id, recipient: u64, archived, pinned: Option<u32>| {
            let mut c = proto::Chat::new();
            c.id = id;
            c.recipientId = recipient;
            c.archived = archived;
            c.pinnedOrder = pinned;
            proto::Frame { item: Some(proto::frame::Item::Chat(c)), ..Default::default() }
        };
        push(chat(10, 1, false, None));
        push(chat(20, 2, false, Some(1)));
        push(chat(30, 3, true, None));
        push(chat(40, 4, false, None));
        let text = |body: &str| {
            let mut t = proto::Text::new();
            t.body = body.into();
            t
        };
        let item = |chat, author, sent: u64, incoming: bool, item: Item| {
            let mut i = proto::ChatItem::new();
            i.chatId = chat;
            i.authorId = author;
            i.dateSent = sent;
            i.directionalDetails = Some(if incoming {
                let mut d = IncomingMessageDetails::new();
                d.dateReceived = sent + 5;
                d.read = true;
                DirectionalDetails::Incoming(d)
            } else {
                let mut d = OutgoingMessageDetails::new();
                d.dateReceived = sent;
                let mut status = proto::SendStatus::new();
                status.deliveryStatus = Some(proto::send_status::DeliveryStatus::Read(Default::default()));
                d.sendStatus.push(status);
                DirectionalDetails::Outgoing(d)
            });
            i.item = Some(item);
            i
        };
        // Alice, in the group, mentioning us.
        let mut mention = text("hi \u{FFFC}!");
        let mut range = proto::BodyRange::new();
        range.start = 3;
        range.length = 1;
        range.associatedValue = Some(proto::body_range::AssociatedValue::MentionAci([0x11; 16].to_vec()));
        mention.bodyRanges.push(range);
        let mut standard = proto::StandardMessage::new();
        standard.text = Some(mention).into();
        // Its whole text is an attachment still to download.
        let mut locator = proto::file_pointer::LocatorInfo::new();
        locator.key = vec![4; 64];
        locator.size = 3_000;
        locator.transitCdnKey = Some("text".into());
        locator.transitCdnNumber = Some(3);
        locator.integrityCheck = Some(proto::file_pointer::locator_info::IntegrityCheck::EncryptedDigest(vec![5; 32]));
        let mut long_text = proto::FilePointer::new();
        long_text.contentType = Some("text/x-signal-plain".into());
        long_text.locatorInfo = Some(locator).into();
        standard.longText = Some(long_text).into();
        // Edited: the item is the latest version; its first version was sent at 800.
        let mut first = proto::StandardMessage::new();
        first.text = Some(text("hi")).into();
        let mut edited = item(30, 2, 1_000, true, Item::StandardMessage(standard));
        edited.revisions.push(item(30, 2, 800, true, Item::StandardMessage(first)));
        push(proto::Frame { item: Some(proto::frame::Item::ChatItem(edited)), ..Default::default() });
        // A sticker pack installed on the phone.
        let mut pack = proto::StickerPack::new();
        pack.packId = vec![0xab; 16];
        pack.packKey = vec![0xcd; 32];
        push(proto::Frame { item: Some(proto::frame::Item::StickerPack(pack)), ..Default::default() });
        // Our reply to Alice, quoting her, with her reaction and a photo
        // still on the transit tier.
        let mut standard = proto::StandardMessage::new();
        standard.text = Some(text("on my way")).into();
        let mut locator = proto::file_pointer::LocatorInfo::new();
        locator.key = vec![1; 64];
        locator.size = 1234;
        locator.transitCdnKey = Some("abc".into());
        locator.transitCdnNumber = Some(3);
        locator.integrityCheck = Some(proto::file_pointer::locator_info::IntegrityCheck::EncryptedDigest(vec![2; 32]));
        let mut photo = proto::FilePointer::new();
        photo.contentType = Some("image/jpeg".into());
        photo.locatorInfo = Some(locator).into();
        let mut photo_attachment = proto::MessageAttachment::new();
        photo_attachment.pointer = Some(photo).into();
        standard.attachments.push(photo_attachment);
        // A video only in the backup (media) tier, and a file the phone
        // never uploaded.
        let mut locator = proto::file_pointer::LocatorInfo::new();
        locator.key = vec![6; 64];
        locator.size = 9_000_000;
        locator.mediaTierCdnNumber = Some(3);
        locator.integrityCheck = Some(proto::file_pointer::locator_info::IntegrityCheck::PlaintextHash(vec![7; 32]));
        let mut video = proto::FilePointer::new();
        video.contentType = Some("video/mp4".into());
        video.locatorInfo = Some(locator).into();
        let mut video_attachment = proto::MessageAttachment::new();
        video_attachment.pointer = Some(video).into();
        standard.attachments.push(video_attachment);
        let mut locator = proto::file_pointer::LocatorInfo::new();
        locator.key = vec![8; 64];
        locator.size = 10;
        let mut file = proto::FilePointer::new();
        file.contentType = Some("application/pdf".into());
        file.locatorInfo = Some(locator).into();
        let mut file_attachment = proto::MessageAttachment::new();
        file_attachment.pointer = Some(file).into();
        standard.attachments.push(file_attachment);
        let mut quote = proto::Quote::new();
        quote.targetSentTimestamp = Some(900);
        quote.authorId = 2;
        quote.text = Some(text("where are you?")).into();
        standard.quote = Some(quote).into();
        let mut reaction = proto::Reaction::new();
        reaction.emoji = "👍".into();
        reaction.authorId = 2;
        reaction.sentTimestamp = 2_100;
        standard.reactions.push(reaction);
        push(proto::Frame { item: Some(proto::frame::Item::ChatItem(item(20, 1, 2_000, false, Item::StandardMessage(standard)))), ..Default::default() });
        // A release note in the phone's release notes chat.
        let mut note = proto::StandardMessage::new();
        note.text = Some(text("Something new")).into();
        push(proto::Frame { item: Some(proto::frame::Item::ChatItem(item(40, 4, 1_500, true, Item::StandardMessage(note)))), ..Default::default() });
        // Deleted, then one that has already disappeared.
        push(proto::Frame {
            item: Some(proto::frame::Item::ChatItem(item(20, 2, 3_000, true, Item::RemoteDeletedMessage(Default::default())))),
            ..Default::default()
        });
        let mut gone = item(10, 1, 4_000, false, Item::StandardMessage({
            let mut s = proto::StandardMessage::new();
            s.text = Some(text("disappeared")).into();
            s
        }));
        gone.expiresInMs = Some(1_000);
        gone.expireStartDate = Some(4_000);
        push(proto::Frame { item: Some(proto::frame::Item::ChatItem(gone)), ..Default::default() });
        // Our poll in the group: Alice chose both options, we the second;
        // then we ended it.
        let mut poll = proto::Poll::new();
        poll.question = "Trail?".into();
        poll.allowMultiple = true;
        for (name, voters) in [("Ridge", vec![(2, 1)]), ("Lake", vec![(2, 1), (1, 2)])] {
            let mut option = proto::poll::PollOption::new();
            option.option = name.into();
            for (voter, count) in voters {
                let mut vote = proto::poll::poll_option::PollVote::new();
                vote.voterId = voter;
                vote.voteCount = count;
                option.votes.push(vote);
            }
            poll.options.push(option);
        }
        poll.hasEnded = true;
        // It reached Alice.
        let mut ours = item(30, 1, 5_000, false, Item::Poll(poll));
        if let Some(DirectionalDetails::Outgoing(d)) = &mut ours.directionalDetails {
            d.sendStatus[0].recipientId = 2;
        }
        push(proto::Frame { item: Some(proto::frame::Item::ChatItem(ours)), ..Default::default() });
        let mut end = proto::PollTerminateUpdate::new();
        end.targetSentTimestamp = 5_000;
        end.question = "Trail?".into();
        let mut update = proto::ChatUpdateMessage::new();
        update.update = Some(proto::chat_update_message::Update::PollTerminate(end));
        let mut ended = item(30, 1, 6_000, false, Item::UpdateMessage(update));
        ended.directionalDetails = Some(DirectionalDetails::Directionless(Default::default()));
        push(proto::Frame { item: Some(proto::frame::Item::ChatItem(ended)), ..Default::default() });
        out
    }

    #[tokio::test]
    async fn imports_chats_and_messages() {
        let db = AppDb::open_in_memory().await.unwrap();
        let me = ACI.service_id_string();
        let plain = sample_backup();
        let mut seen = Vec::new();
        let imported = import(&db, &plain, &me, |done| seen.push(done)).await.unwrap();
        // Progress only moves forward and ends at the whole backup.
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(seen.last(), Some(&(plain.len() as u64)));
        assert_eq!((imported.chats, imported.messages), (4, 6));
        assert_eq!(imported.sticker_packs, vec![(vec![0xab; 16], vec![0xcd; 32])]);
        // Each setting the backup leaves off is off, as Desktop imports them.
        let settings = imported.settings.clone().unwrap();
        let on = |s: crate::AccountSetting| settings.flags[&s];
        assert!(on(crate::AccountSetting::ReadReceipts) && on(crate::AccountSetting::TypingIndicators));
        assert!(!on(crate::AccountSetting::LinkPreviews) && !on(crate::AccountSetting::SealedSenderIndicators));
        assert!(on(crate::AccountSetting::KeepMutedChatsArchived) && !on(crate::AccountSetting::PreferContactAvatars));
        // The timer and who sees our number, and nothing the backup doesn't carry.
        assert_eq!((settings.universal_expire_timer, settings.phone_number_sharing), (Some(604_800), Some(true)));
        assert!(!settings.flags.contains_key(&crate::AccountSetting::ReactionNotifications));
        assert_eq!(db.setting(crate::SETTING_THEME).await.unwrap().as_deref(), Some("dark"));
        assert_eq!(db.setting(crate::SETTING_PHONE_NUMBER_DISCOVERABLE).await.unwrap().as_deref(), Some("false"));

        let alice_sid = Aci::from_uuid_bytes([0xa1; 16]).service_id_string();
        let alice = db.conversation(&ConversationId::direct(&alice_sid)).await.unwrap().unwrap();
        assert_eq!((alice.title.as_str(), alice.phone.as_deref(), alice.pinned), ("Alice Liddell", Some("+15550100"), true));
        let group = db.conversation(&ConversationId::group(&[9; 32])).await.unwrap().unwrap();
        assert_eq!((group.title.as_str(), group.archived, group.members.clone()), ("Hikers", true, vec![alice_sid.clone()]));
        assert!(group.ended, "the snapshot says it was ended");
        // Whose stories we take, and the group's story kept on the list.
        assert!(db.profile_sharing(&alice.id).await.unwrap());
        assert!(!db.profile_sharing(&group.id).await.unwrap());
        assert_eq!(db.story_send_mode(&group.id).await.unwrap(), signal_store::StorySendMode::Always);
        let note = db.conversation(&ConversationId::direct(&me)).await.unwrap().unwrap();
        assert_eq!(note.kind, ConversationKind::NoteToSelf);
        assert_eq!(db.messages(&note.id, None, 10).await.unwrap().len(), 0, "the disappeared message isn't imported");

        let in_group = db.messages(&group.id, None, 10).await.unwrap();
        assert_eq!(in_group[0].body, "hi @You!");
        assert_eq!(in_group[0].mentions[0].aci, me);
        assert_eq!(in_group[0].author.as_deref(), Some(alice_sid.as_str()));
        assert_eq!(group.unread_count, 0, "read on the phone");
        // Dated by its first version; the edit's date names it too.
        assert_eq!((in_group[0].sent_at, in_group[0].edited), (800, true));
        let by_edit = db.find_message(&group.id, Some(&alice_sid), 1_000).await.unwrap().map(|m| m.id);
        assert_eq!(by_edit, Some(in_group[0].id));
        // Its first version's own text is its edit history.
        let history = db.edit_history(in_group[0].id).await.unwrap();
        assert_eq!(history.iter().map(|r| (r.sent_at, r.body.as_str())).collect::<Vec<_>>(), [(800, "hi")]);
        // Its whole text downloads next, its mention kept for it.
        let long_text = in_group[0].long_text.as_ref().unwrap();
        assert_eq!((long_text.size, long_text.remote.cdn_key.as_deref()), (3_000, Some("text")));
        assert_eq!(long_text.mentions, vec![me.clone()]);
        assert_eq!(imported.long_texts, [in_group[0].id]);
        // The poll, with each voter's choices, ended, and its notice.
        let poll = in_group[1].poll.clone().unwrap();
        assert_eq!((in_group[1].body.as_str(), poll.options.len(), poll.allow_multiple, poll.ended), ("Trail?", 2, true, true));
        let tally = poll.tally();
        assert_eq!(tally.voters, vec![vec![Some(alice_sid.clone())], vec![Some(alice_sid.clone()), None]]);
        assert_eq!(poll.next_vote_count(), 3, "we had voted twice");
        assert_eq!(poll.reached(&alice_sid), Some(true), "only who it reached may vote");
        assert_eq!(
            in_group[2].notice,
            Some(signal_store::Notice::PollEnded { by: None, question: "Trail?".into(), sent_at: 5_000 })
        );

        let with_alice = db.messages(&alice.id, None, 10).await.unwrap();
        let reply = &with_alice[0];
        assert_eq!((reply.direction, reply.status, reply.body.as_str()), (Direction::Outgoing, DeliveryStatus::Read, "on my way"));
        assert_eq!(reply.quote.as_ref().map(|q| (q.sent_at, q.author_name.as_str())), Some((900, "Alice Liddell")));
        assert_eq!(reply.reactions[0].emoji, "👍");
        // The photo keeps where to download it, and downloads now.
        let photo = &reply.attachments[0];
        assert_eq!(photo.size, 1234);
        let remote = photo.remote.as_ref().unwrap();
        assert_eq!((remote.cdn, remote.cdn_key.as_deref(), remote.key.len()), (3, Some("abc"), 128));
        assert_eq!(remote.digest.as_deref(), Some(hex::encode([2; 32]).as_str()));
        assert_eq!(remote.media_tier_cdn, None);
        // The video is only in the backup tier: named by its key and
        // plaintext hash, on the CDN the backup gave.
        let video = reply.attachments[1].remote.as_ref().unwrap();
        assert_eq!(video.cdn_key, None);
        assert_eq!(video.plaintext_hash.as_deref(), Some(hex::encode([7; 32]).as_str()));
        assert_eq!((video.media_tier_cdn, video.digest.as_ref()), (Some(3), None));
        // The file can't be checked anywhere, so it has nowhere to come from.
        assert_eq!(reply.attachments[2].remote, None);
        // The photo whole; the video's backup thumbnail.
        assert_eq!(imported.downloads, vec![(reply.id, 0), (reply.id, 1)]);
        assert!(with_alice[1].deleted);

        // The phone's release notes are the Signal chat's.
        let signal = db.conversation(&ConversationId::signal()).await.unwrap().unwrap();
        assert_eq!(signal.title, "Signal");
        let notes = db.messages(&signal.id, None, 10).await.unwrap();
        assert_eq!((notes[0].body.as_str(), notes[0].author.as_deref()), ("Something new", Some(signal_store::SIGNAL_ACI)));
    }

    #[test]
    fn chat_updates_become_notices_as_desktop_imports_them() {
        use proto::chat_update_message::Update;
        use proto::simple_chat_update::Type;
        use signal_store::{GroupNotice, Inviter, Notice};
        let me = Aci::from_uuid_bytes([0x11; 16]).service_id_string();
        let alice = Aci::from_uuid_bytes([0xa1; 16]).service_id_string();
        let bob = Aci::from_uuid_bytes([0xb0; 16]).service_id_string();
        let direct = ConversationId::direct(&alice);
        let group = ConversationId::group(&[1; 32]);
        let wrap = |update: Update| {
            let mut message = proto::ChatUpdateMessage::new();
            message.update = Some(update);
            message
        };
        let simple = |kind: Type| {
            let mut simple = proto::SimpleChatUpdate::new();
            simple.type_ = kind.into();
            wrap(Update::SimpleUpdate(simple))
        };
        let nobody = |_: u64| None;
        let notices = |update: &proto::ChatUpdateMessage, author: Option<Option<String>>, chat: &ConversationId| {
            update_notices(update, author, chat, &me, nobody)
        };
        let theirs = Some(Some(alice.clone()));
        assert_eq!(notices(&simple(Type::IDENTITY_UPDATE), None, &direct), [Notice::SafetyNumberChanged { who: alice.clone() }]);
        assert_eq!(notices(&simple(Type::IDENTITY_UPDATE), theirs.clone(), &group), [Notice::SafetyNumberChanged { who: alice.clone() }]);
        assert_eq!(
            notices(&simple(Type::IDENTITY_VERIFIED), theirs.clone(), &direct),
            [Notice::Verified { who: alice.clone(), verified: true, local: false }]
        );
        assert_eq!(
            notices(&simple(Type::IDENTITY_DEFAULT), theirs.clone(), &direct),
            [Notice::Verified { who: alice.clone(), verified: false, local: false }]
        );
        assert_eq!(notices(&simple(Type::CHANGE_NUMBER), theirs.clone(), &group), [Notice::NumberChanged { who: alice.clone() }]);
        assert_eq!(notices(&simple(Type::CHAT_SESSION_REFRESH), theirs.clone(), &direct), [Notice::SessionRefreshed]);
        assert_eq!(notices(&simple(Type::BAD_DECRYPT), theirs.clone(), &group), [Notice::DeliveryIssue { who: alice.clone() }]);
        assert_eq!(
            notices(&simple(Type::UNSUPPORTED_PROTOCOL_MESSAGE), Some(None), &direct),
            [Notice::UnsupportedMessage { who: None }]
        );
        assert_eq!(notices(&simple(Type::JOINED_SIGNAL), theirs.clone(), &direct), []);

        let mut profile = proto::ProfileChangeChatUpdate::new();
        profile.previousName = "Al".into();
        profile.newName = "Alice".into();
        assert_eq!(
            notices(&wrap(Update::ProfileChange(profile)), theirs.clone(), &direct),
            [Notice::ProfileNameChanged { who: alice.clone(), old_name: "Al".into(), new_name: "Alice".into() }]
        );
        let mut timer = proto::ExpirationTimerChatUpdate::new();
        timer.expiresInMs = 86_400_000;
        assert_eq!(notices(&wrap(Update::ExpirationTimerChange(timer)), Some(None), &direct), [Notice::Timer { by: None, seconds: 86_400 }]);

        // A group change's details, in order; ones naming nobody are left out.
        let mut change = proto::GroupChangeChatUpdate::new();
        let detail = |u: proto::group_change_chat_update::update::Update| {
            let mut detail = proto::group_change_chat_update::Update::new();
            detail.update = Some(u);
            detail
        };
        use proto::group_change_chat_update::update::Update as U;
        let mut name = proto::GroupNameUpdate::new();
        name.updaterAci = Some(vec![0xa1; 16]);
        name.newGroupName = Some("Hikers".into());
        let mut left = proto::GroupMemberLeftUpdate::new();
        left.aci = vec![0xb0; 16];
        let mut accepted = proto::GroupInvitationAcceptedUpdate::new();
        accepted.newMemberAci = vec![0xb0; 16];
        accepted.inviterAci = Some(vec![0x11; 16]);
        let anonymous = proto::GroupAvatarUpdate::new();
        let mut ended = proto::GroupTerminateChangeUpdate::new();
        ended.updaterAci = Some(vec![0x11; 16]);
        change.updates = vec![
            detail(U::GroupNameUpdate(name)),
            detail(U::GroupMemberLeftUpdate(left)),
            detail(U::GroupInvitationAcceptedUpdate(accepted)),
            detail(U::GroupAvatarUpdate(anonymous)),
            detail(U::GroupTerminateChangeUpdate(ended)),
        ];
        assert_eq!(
            notices(&wrap(Update::GroupChange(change)), None, &group),
            [
                Notice::Group { by: Some(alice.clone()), change: GroupNotice::Title { title: "Hikers".into() } },
                Notice::Group { by: Some(bob.clone()), change: GroupNotice::Removed { member: Some(bob.clone()) } },
                Notice::Group { by: Some(bob.clone()), change: GroupNotice::AcceptedInvite { member: Some(bob.clone()), inviter: Inviter::Us } },
                Notice::Group { by: None, change: GroupNotice::Ended },
            ]
        );
    }

    #[test]
    fn message_request_answers_and_address_book_names_are_read() {
        use proto::simple_chat_update::Type;
        use signal_store::MessageRequestEvent;
        let update = |kind: Type| {
            let mut simple = proto::SimpleChatUpdate::new();
            simple.type_ = kind.into();
            let mut update = proto::ChatUpdateMessage::new();
            update.update = Some(proto::chat_update_message::Update::SimpleUpdate(simple));
            update
        };
        assert_eq!(message_request_event(&update(Type::MESSAGE_REQUEST_ACCEPTED)), Some(MessageRequestEvent::Accept));
        assert_eq!(message_request_event(&update(Type::REPORTED_SPAM)), Some(MessageRequestEvent::Spam));
        assert_eq!(message_request_event(&update(Type::BLOCKED)), Some(MessageRequestEvent::Block));
        assert_eq!(message_request_event(&update(Type::UNBLOCKED)), Some(MessageRequestEvent::Unblock));
        assert_eq!(message_request_event(&update(Type::JOINED_SIGNAL)), None);

        let mut contact = proto::Contact::new();
        contact.aci = Some(vec![0xa1; 16]);
        contact.profileGivenName = Some("Alice".into());
        let recipient = |contact: proto::Contact| {
            let mut r = proto::Recipient::new();
            r.destination = Some(proto::recipient::Destination::Contact(contact));
            r
        };
        assert_eq!(system_contact(&recipient(contact.clone())), None, "a profile name isn't the address book's");
        contact.systemGivenName = "Alice".into();
        assert_eq!(system_contact(&recipient(contact)), Some(Aci::from_uuid_bytes([0xa1; 16]).service_id_string()));
    }

    #[test]
    fn backup_chat_folders_read_as_desktops_import_reads_them() {
        let people = HashMap::from([
            (1u64, Who::Me),
            (2, Who::Contact { sid: "a".into(), name: "A".into(), e164: None, blocked: false }),
            (3, Who::Group { key: vec![7; 32], title: String::new(), members: Vec::new(), blocked: false, ended: false }),
        ]);
        let folder = proto::ChatFolder {
            id: vec![0x11; 16],
            name: "Hikers".into(),
            folderType: protobuf::EnumOrUnknown::new(proto::chat_folder::FolderType::CUSTOM),
            includedRecipientIds: vec![1, 2, 9],
            excludedRecipientIds: vec![3],
            showMutedChats: true,
            ..Default::default()
        };
        let read = chat_folder(&folder, &people, "me", 4).unwrap();
        assert_eq!((read.name.as_str(), read.position, read.show_muted_chats), ("Hikers", 4, true));
        assert_eq!(read.included, [ConversationId::direct("me"), ConversationId::direct("a")]);
        assert_eq!(read.excluded, [ConversationId::group(&[7; 32])]);
        let unknown = proto::ChatFolder { folderType: protobuf::EnumOrUnknown::new(proto::chat_folder::FolderType::UNKNOWN), ..folder.clone() };
        assert!(chat_folder(&unknown, &people, "me", 0).is_none());
        assert!(chat_folder(&proto::ChatFolder { id: vec![1; 3], ..folder }, &people, "me", 0).is_none());
    }

    #[test]
    fn chat_styles_read_as_desktops_import_reads_them() {
        use proto::chat_style::{BubbleColor, BubbleColorPreset, Gradient, custom_chat_color::Color};
        // Pure red: hue 0, saturation 100%, lightness a half.
        let red = desktop_hsl(0xffff0000);
        assert_eq!((red.hue, red.saturation, red.lightness), (0., 100., Some(0.5)));
        let blue = desktop_hsl(0xff0000ff);
        assert_eq!(blue.hue, 240.);
        assert_eq!(custom_color(&Color::Solid(0xffff0000)).unwrap().end, None);
        // A backup's 0 degrees (bottom to top) is Desktop's 270.
        let gradient = Gradient { angle: 0, colors: vec![0xffff0000, 0xff00ff00, 0xff0000ff], ..Default::default() };
        let color = custom_color(&Color::Gradient(gradient)).unwrap();
        assert_eq!((color.deg, color.end.map(|e| e.hue)), (Some(270.), Some(240.)));
        assert!(custom_color(&Color::Gradient(Gradient::default())).is_none(), "no colors");

        let custom = HashMap::from([(7u64, ("c7".to_owned(), color))]);
        let style = |bubble| proto::ChatStyle { bubbleColor: Some(bubble), ..Default::default() };
        let preset = |p| style(BubbleColor::BubbleColorPreset(protobuf::EnumOrUnknown::new(p)));
        assert_eq!(chat_color(&preset(BubbleColorPreset::GRADIENT_SEA), &custom), Some(signal_store::ChatColor::preset("sea")));
        assert_eq!(chat_color(&preset(BubbleColorPreset::SOLID_CRIMSON), &custom), Some(signal_store::ChatColor::preset("crimson")));
        assert_eq!(
            chat_color(&preset(BubbleColorPreset::UNKNOWN_BUBBLE_COLOR_PRESET), &custom),
            Some(signal_store::ChatColor::preset("ultramarine"))
        );
        assert_eq!(
            chat_color(&style(BubbleColor::CustomColorId(7)), &custom),
            Some(signal_store::ChatColor::Custom { id: "c7".into(), value: color })
        );
        assert_eq!(chat_color(&style(BubbleColor::CustomColorId(8)), &custom), None, "missing: automatic");
        assert_eq!(chat_color(&style(BubbleColor::AutoBubbleColor(Default::default())), &custom), None);
        assert_eq!(chat_color(&proto::ChatStyle::default(), &custom), None);
    }
}
