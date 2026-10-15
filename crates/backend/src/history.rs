//! Message history at link time ("link and sync"), as Signal Desktop does
//! it: the phone uploads an encrypted backup for the new device, which
//! fetches it, decrypts it with the one-time key from linking, and imports
//! chats and messages.
//!
//! The file: a 16-byte IV, AES-256-CBC ciphertext and an HMAC-SHA256 (the
//! attachment format), wrapping gzip, wrapping length-prefixed protobufs: a
//! `BackupInfo`, then `Frame`s (recipients, chats, then chat items).
//!
//! Attachments come over as metadata only; Desktop fetches their files in
//! a separate step that isn't here yet.

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
    let path = format!("attachments/{}", remote.cdn_key);
    let response = service.request(reqwest::Method::GET, Endpoint::cdn(remote.cdn, path), HttpAuthOverride::Unidentified)?.send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("it's no longer on Signal's servers");
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
    /// Photos and GIFs to download now, as `(message id, attachment index)`;
    /// other files wait to be opened.
    pub downloads: Vec<(i64, usize)>,
    /// Installed sticker packs: `(pack id, pack key)`.
    pub sticker_packs: Vec<(Vec<u8>, Vec<u8>)>,
}

enum Who {
    Me,
    Contact { sid: String, name: String, e164: Option<String>, blocked: bool },
    Group { key: Vec<u8>, title: String, members: Vec<String>, blocked: bool },
    Other,
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
            Who::Group { key: group.masterKey.clone(), title, members, blocked: group.blocked }
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

/// Imports a decrypted backup's chats and messages for account `me`.
/// `progress` gets the bytes of `plain` imported so far.
pub(crate) async fn import(db: &AppDb, plain: &[u8], me: &str, mut progress: impl FnMut(u64)) -> anyhow::Result<Imported> {
    let mut people: HashMap<u64, Who> = HashMap::new();
    let mut chats: HashMap<u64, ConversationId> = HashMap::new();
    let mut pinned: Vec<(u32, ConversationId)> = Vec::new();
    let mut states: Vec<ChatState> = Vec::new();
    let mut imported = Imported::default();
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
                    Who::Group { key, title, members, blocked } => (
                        ConversationId::group(key),
                        ConversationUpdate {
                            kind: Some(ConversationKind::Group),
                            title: Some(title.clone()).filter(|t| !t.is_empty()),
                            members: Some(members.clone()),
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
                chats.insert(chat.id, id);
                imported.chats += 1;
            }
            Some(proto::frame::Item::StickerPack(pack)) => {
                imported.sticker_packs.push((pack.packId.clone(), pack.packKey.clone()));
            }
            Some(proto::frame::Item::ChatItem(item)) => {
                imported.messages +=
                    usize::from(import_item(db, &item, &people, &chats, me, now, &mut imported.downloads).await?);
            }
            _ => {}
        }
    }
    db.apply_chat_states(&states).await?;
    if !pinned.is_empty() {
        pinned.sort_by_key(|(order, _)| *order);
        db.replace_pinned(&pinned.into_iter().map(|(_, id)| id).collect::<Vec<_>>()).await?;
    }
    Ok(imported)
}

/// One chat item; `Ok(false)` for kinds that aren't imported (group
/// updates, calls, payments, polls, …) or that have already disappeared.
async fn import_item(
    db: &AppDb,
    item: &proto::ChatItem,
    people: &HashMap<u64, Who>,
    chats: &HashMap<u64, ConversationId>,
    me: &str,
    now: i64,
    downloads: &mut Vec<(i64, usize)>,
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
                    attachment: q.attachments.first().map(|a| {
                        use proto::message_attachment::Flag;
                        let flag = a.thumbnail.as_ref().map(|t| t.flag.enum_value_or_default());
                        signal_store::QuotedAttachment {
                            content_type: a.contentType.clone().unwrap_or_default(),
                            file_name: a.fileName.clone(),
                            voice: flag == Some(Flag::VOICE_MESSAGE),
                            gif: flag == Some(Flag::GIF),
                        }
                    }),
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
    for (index, attachment) in message.attachments.iter().enumerate() {
        let photo = attachment.gif || attachment.voice || attachment.content_type.starts_with("image/");
        if attachment.remote.is_some() && photo && attachment.size <= crate::actor::AUTO_DOWNLOAD_MAX {
            downloads.push((id, index));
        }
    }
    for at in &edits {
        db.edit_message(id, &message.body, *at).await?;
    }
    if !spans.is_empty() {
        db.set_mentions(id, &spans).await?;
    }
    if !style_spans.is_empty() {
        db.set_styles(id, &style_spans).await?;
    }
    if deleted {
        db.mark_deleted(id).await?;
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
        db.pin_message(conversation, id, pin.pinnedAtTimestamp as i64, expires, crate::MAX_PINNED_MESSAGES).await?;
    }
    Ok(true)
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
    }
}

/// Where a transferred attachment can be downloaded: the transit tier,
/// which the phone fills for link and sync (and which expires). The backup
/// (media) tier needs a backup subscription's credentials, so it isn't used.
fn remote(locator: &proto::file_pointer::LocatorInfo) -> Option<RemoteAttachment> {
    use proto::file_pointer::locator_info::IntegrityCheck;
    let cdn_key = locator.transitCdnKey.clone().filter(|k| !k.is_empty())?;
    if locator.key.len() != 64 {
        return None;
    }
    let (digest, plaintext_hash) = match &locator.integrityCheck {
        Some(IntegrityCheck::EncryptedDigest(d)) => (Some(hex::encode(d)), None),
        Some(IntegrityCheck::PlaintextHash(h)) => (None, Some(hex::encode(h))),
        _ => return None,
    };
    Some(RemoteAttachment { cdn: locator.transitCdnNumber.unwrap_or(0), cdn_key, key: hex::encode(&locator.key), digest, plaintext_hash })
}

/// A backup's text styles (as on the wire: UTF-16, over the body with its
/// mention placeholders).
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
        push(recipient(2, Destination::Contact(contact)));
        let mut group = proto::Group::new();
        group.masterKey = vec![9; 32];
        let mut snapshot = proto::group::GroupSnapshot::new();
        let mut title = proto::group::GroupAttributeBlob::new();
        title.content = Some(proto::group::group_attribute_blob::Content::Title("Hikers".into()));
        snapshot.title = Some(title).into();
        let mut member = proto::group::Member::new();
        member.userId = alice.to_vec();
        snapshot.members.push(member);
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
        assert_eq!((imported.chats, imported.messages), (4, 4));
        assert_eq!(imported.sticker_packs, vec![(vec![0xab; 16], vec![0xcd; 32])]);

        let alice_sid = Aci::from_uuid_bytes([0xa1; 16]).service_id_string();
        let alice = db.conversation(&ConversationId::direct(&alice_sid)).await.unwrap().unwrap();
        assert_eq!((alice.title.as_str(), alice.phone.as_deref(), alice.pinned), ("Alice Liddell", Some("+15550100"), true));
        let group = db.conversation(&ConversationId::group(&[9; 32])).await.unwrap().unwrap();
        assert_eq!((group.title.as_str(), group.archived, group.members.clone()), ("Hikers", true, vec![alice_sid.clone()]));
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

        let with_alice = db.messages(&alice.id, None, 10).await.unwrap();
        let reply = &with_alice[0];
        assert_eq!((reply.direction, reply.status, reply.body.as_str()), (Direction::Outgoing, DeliveryStatus::Read, "on my way"));
        assert_eq!(reply.quote.as_ref().map(|q| (q.sent_at, q.author_name.as_str())), Some((900, "Alice Liddell")));
        assert_eq!(reply.reactions[0].emoji, "👍");
        // The photo keeps where to download it, and downloads now.
        let photo = &reply.attachments[0];
        assert_eq!(photo.size, 1234);
        let remote = photo.remote.as_ref().unwrap();
        assert_eq!((remote.cdn, remote.cdn_key.as_str(), remote.key.len()), (3, "abc", 128));
        assert_eq!(remote.digest.as_deref(), Some(hex::encode([2; 32]).as_str()));
        assert_eq!(imported.downloads, vec![(reply.id, 0)]);
        assert!(with_alice[1].deleted);

        // The phone's release notes are the Signal chat's.
        let signal = db.conversation(&ConversationId::signal()).await.unwrap().unwrap();
        assert_eq!(signal.title, "Signal");
        let notes = db.messages(&signal.id, None, 10).await.unwrap();
        assert_eq!((notes[0].body.as_str(), notes[0].author.as_deref()), ("Something new", Some(signal_store::SIGNAL_ACI)));
    }
}
