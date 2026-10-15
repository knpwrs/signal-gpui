//! Storage Service: the account's encrypted state (contacts, groups, account
//! settings), kept by the primary device. Signal Desktop takes most display
//! names, profile keys, pins and archived/muted state from it; contact sync
//! only carries names from the phone's address book.
//!
//! Pins and per-chat flags (archived, muted, marked unread, blocked) are also
//! written back. Records are immutable on the server: a change
//! uploads the edited record under a new random key, deletes the old one and
//! bumps the manifest version, all in one write that fails with 409 if
//! another device wrote first.

mod wire;

use aes_gcm::{Aes256Gcm, KeyInit as _, aead::Aead as _};
use anyhow::{Context as _, anyhow, bail, ensure};
use base64::Engine as _;
use hkdf::Hkdf;
use hmac::{Hmac, Mac as _};
use presage::{
    libsignal_service::{
        StorageService,
        configuration::{Endpoint, ServiceCredentials},
        master_key::StorageServiceKey,
        prelude::Uuid,
        protocol::{Aci, E164, ServiceId},
        push_service::{HttpAuth, HttpAuthOverride, PushService},
    },
    proto::{
        AccountRecord, ContactRecord, GroupV2Record, ManifestRecord, ReadOperation, StorageItem,
        StorageItems, StorageManifest, StorageRecord, WriteOperation,
        account_record::{PinnedConversation, pinned_conversation::{self, Identifier as PinnedId}},
        manifest_record::{Identifier, identifier::Type as IdentifierType},
        storage_record::Record,
    },
    store::StateStore,
};
use prost::Message as _;
use rand::Rng as _;
use sha2::Sha256;
use signal_store::{ChatState, ConversationId};

use crate::actor::SignalManager;

/// Items per read request (Signal Android reads in batches of 1000).
const READ_BATCH: usize = 1000;

/// What a contact record tells us about one account.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StorageContact {
    pub aci: Uuid,
    /// Display name, in Signal's order: nickname, address-book name, profile name.
    pub name: Option<String>,
    pub profile_key: Option<[u8; 32]>,
    pub e164: Option<String>,
    /// Hidden or blocked on the phone: not offered for new chats.
    pub hidden: bool,
}

/// What we read from the Storage Service.
#[derive(Debug, Default)]
pub(crate) struct StorageSnapshot {
    pub contacts: Vec<StorageContact>,
    /// Pinned conversations in display order; `None` without an account record.
    pub pinned: Option<Vec<ConversationId>>,
    /// Archived/muted state of direct and group chats.
    pub chats: Vec<ChatState>,
    /// The account's "Generate link previews" setting.
    pub link_previews: Option<bool>,
    /// Story lists, My Story among them (not deleted ones).
    pub story_lists: Vec<crate::StoryList>,
    /// Chats whose stories are hidden (`hideStory`).
    pub hidden_stories: Vec<ConversationId>,
    /// The account's `storiesDisabled`.
    pub stories_disabled: Option<bool>,
    /// The account's `storyViewReceiptsEnabled`, when set.
    pub story_view_receipts: Option<bool>,
}

/// Reads the account record and every contact record from the Storage Service.
pub(crate) async fn fetch(manager: &SignalManager) -> anyhow::Result<StorageSnapshot> {
    let master_key = manager
        .store()
        .fetch_master_key()
        .await
        .map_err(|e| anyhow!("{e}"))?
        .context("no master key; relink to read contacts from Storage Service")?;
    let storage = StorageService::new(push_service(manager)?, StorageServiceKey::from_master_key(&master_key))
        .await
        .context("authenticating with Storage Service")?;
    let manifest = storage.manifest().await.context("reading storage manifest")?;
    let ikm = (!manifest.record_ikm.is_empty()).then_some(manifest.record_ikm.as_slice());
    let keys: Vec<Vec<u8>> = manifest
        .identifiers
        .iter()
        .filter(|id| {
            matches!(
                id.r#type(),
                IdentifierType::Contact
                    | IdentifierType::Groupv2
                    | IdentifierType::Account
                    | IdentifierType::StoryDistributionList
            )
        })
        .map(|id| id.raw.clone())
        .collect();

    let mut snapshot = StorageSnapshot::default();
    for batch in keys.chunks(READ_BATCH) {
        let records = storage.read_items(batch.to_vec(), ikm).await.context("reading storage records")?;
        for record in records {
            match record.record {
                Some(Record::Contact(contact)) => {
                    snapshot.chats.extend(contact_chat(&contact));
                    if contact.hide_story
                        && let Some(aci) = record_aci(&contact)
                    {
                        snapshot.hidden_stories.push(ConversationId::direct(&Aci::from(aci).service_id_string()));
                    }
                    snapshot.contacts.extend(from_record(&contact));
                }
                Some(Record::GroupV2(group)) => {
                    if group.hide_story && group.master_key.len() == 32 {
                        snapshot.hidden_stories.push(ConversationId::group(&group.master_key));
                    }
                    snapshot.chats.extend(group_chat(&group));
                }
                Some(Record::Account(account)) => {
                    snapshot.pinned = Some(pinned(&account));
                    snapshot.link_previews = Some(account.link_previews);
                    snapshot.stories_disabled = Some(account.stories_disabled);
                    snapshot.story_view_receipts = match account.story_view_receipts_enabled() {
                        presage::proto::OptionalBool::Enabled => Some(true),
                        presage::proto::OptionalBool::Disabled => Some(false),
                        presage::proto::OptionalBool::Unset => None,
                    };
                }
                Some(Record::StoryDistributionList(list)) => snapshot.story_lists.extend(story_list(&list)),
                _ => {}
            }
        }
    }
    Ok(snapshot)
}

/// The account's pinned conversations, in order. Legacy (v1) groups, release
/// notes and number-only contacts have no conversation here and are skipped.
pub(crate) fn pinned(account: &AccountRecord) -> Vec<ConversationId> {
    account.pinned_conversations.iter().filter_map(pinned_id).collect()
}

fn pinned_id(pin: &PinnedConversation) -> Option<ConversationId> {
    match pin.identifier.as_ref()? {
        PinnedId::Contact(contact) => {
            let sid = ServiceId::parse_from_service_id_binary(&contact.service_id_binary)
                .or_else(|| ServiceId::parse_from_service_id_string(&contact.service_id))?;
            Some(ConversationId::direct(&sid.service_id_string()))
        }
        PinnedId::GroupMasterKey(key) if key.len() == 32 => Some(ConversationId::group(key)),
        // The Signal chat (Desktop pins it as `releaseNotes`).
        PinnedId::ReleaseNotes(_) => Some(ConversationId::signal()),
        _ => None,
    }
}

fn pin_for(id: &ConversationId) -> Option<PinnedConversation> {
    let identifier = if id.is_signal() {
        PinnedId::ReleaseNotes(Default::default())
    } else if let Some(key) = id.as_group() {
        PinnedId::GroupMasterKey(key)
    } else {
        let sid = ServiceId::parse_from_service_id_string(id.as_direct()?)?;
        // Readers prefer the binary form; the string is for older clients.
        PinnedId::Contact(pinned_conversation::Contact {
            service_id: sid.service_id_string(),
            service_id_binary: sid.service_id_binary(),
            ..Default::default()
        })
    };
    Some(PinnedConversation { identifier: Some(identifier) })
}

/// Signal's limit (`MAX_PINNED_CONVERSATIONS` in Desktop and Android).
pub const MAX_PINNED: usize = 4;

const RECORD_ACCOUNT: u32 = 4; // StorageRecord.account
const ACCOUNT_PINNED: u32 = 14; // AccountRecord.pinnedConversations

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PinEdit {
    Pin(ConversationId),
    Unpin(ConversationId),
}

/// Applies `edit` to an encoded `StorageRecord` holding the account record,
/// changing only its pinned-conversation entries. `Ok(None)` when there is
/// nothing to change.
pub(crate) fn edit_pins(record: &[u8], edit: &PinEdit) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(record).context("malformed storage record")?;
    let account = fields
        .iter()
        .find(|f| f.number == RECORD_ACCOUNT)
        .and_then(|f| f.payload)
        .context("storage record is not an account record")?;
    let Some(account) = edit_account_pins(account, edit)? else { return Ok(None) };
    let mut out = Vec::with_capacity(record.len() + 64);
    for field in &fields {
        if field.number == RECORD_ACCOUNT {
            wire::put_len(&mut out, RECORD_ACCOUNT, &account);
        } else {
            out.extend_from_slice(field.raw);
        }
    }
    Ok(Some(out))
}

fn edit_account_pins(account: &[u8], edit: &PinEdit) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(account).context("malformed account record")?;
    let pin_of = |field: &wire::Field| {
        let payload = field.payload.filter(|_| field.number == ACCOUNT_PINNED)?;
        PinnedConversation::decode(payload).ok().as_ref().and_then(pinned_id)
    };
    let pins = fields.iter().filter(|f| f.number == ACCOUNT_PINNED).count();
    let (target, pinning) = match edit {
        PinEdit::Pin(id) => (id, true),
        PinEdit::Unpin(id) => (id, false),
    };
    let present = fields.iter().any(|f| pin_of(f).as_ref() == Some(target));
    if present == pinning {
        return Ok(None);
    }
    ensure!(!pinning || pins < MAX_PINNED, "you can pin up to {MAX_PINNED} chats");

    let mut out = Vec::with_capacity(account.len() + 64);
    for field in &fields {
        // Entries we can't map (legacy groups) are kept as-is.
        if pinning || pin_of(field).as_ref() != Some(target) {
            out.extend_from_slice(field.raw);
        }
    }
    if pinning {
        let pin = pin_for(target).context("can't pin this conversation")?;
        wire::put_len(&mut out, ACCOUNT_PINNED, &pin.encode_to_vec());
    }
    Ok(Some(out))
}

/// Pins or unpins a conversation for the whole account.
pub(crate) async fn write_pin(manager: &SignalManager, edit: &PinEdit) -> anyhow::Result<()> {
    let session = Session::new(manager).await?;
    // A conflict means another device wrote in between: re-read and retry.
    for _ in 0..3 {
        let manifest = session.manifest().await?;
        let ikm = manifest_ikm(&manifest);
        let old_key = manifest
            .identifiers
            .iter()
            .find(|id| id.r#type() == IdentifierType::Account)
            .map(|id| id.raw.clone())
            .context("storage has no account record")?;
        let record = session.read_raw(&old_key, ikm).await?;
        let Some(record) = edit_pins(&record, edit)? else { return Ok(()) };
        if session.commit(manager, &manifest, Some(&old_key), IdentifierType::Account, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
}

const RECORD_CONTACT: u32 = 1; // StorageRecord.contact
const RECORD_GROUP_V2: u32 = 3; // StorageRecord.groupV2

/// A change to one chat's contact or group record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ChatEdit {
    Archived(bool),
    /// Unix millis; `u64::MAX` is "always", 0 unmutes.
    MutedUntil(u64),
    MarkedUnread(bool),
    Blocked(bool),
    /// Their stories hidden (`hideStory`).
    HideStory(bool),
}

impl ChatEdit {
    /// The field number in a `ContactRecord` or `GroupV2Record`, and its value.
    fn field(self, group: bool) -> (u32, u64) {
        let (contact, group_field, value) = match self {
            ChatEdit::Blocked(on) => (9, 2, u64::from(on)),
            ChatEdit::Archived(on) => (11, 4, u64::from(on)),
            ChatEdit::MarkedUnread(on) => (12, 5, u64::from(on)),
            ChatEdit::MutedUntil(until) => (13, 6, until),
            ChatEdit::HideStory(on) => (14, 8, u64::from(on)),
        };
        (if group { group_field } else { contact }, value)
    }
}

/// Applies `edit` to an encoded `StorageRecord` holding a contact or group
/// record, changing only that one field. `Ok(None)` when it's already set.
pub(crate) fn edit_chat_record(record: &[u8], edit: ChatEdit) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(record).context("malformed storage record")?;
    let (kind, chat) = fields
        .iter()
        .find_map(|f| Some((f.number, f.payload?)).filter(|(n, _)| matches!(*n, RECORD_CONTACT | RECORD_GROUP_V2)))
        .context("storage record is not a contact or group")?;
    let (number, value) = edit.field(kind == RECORD_GROUP_V2);
    let chat_fields = wire::fields(chat).context("malformed chat record")?;
    // proto3: the last occurrence wins, and absent means zero.
    let current = chat_fields.iter().rev().find(|f| f.number == number).and_then(|f| f.varint).unwrap_or(0);
    if current == value {
        return Ok(None);
    }
    let mut edited = Vec::with_capacity(chat.len() + 12);
    for field in chat_fields.iter().filter(|f| f.number != number) {
        edited.extend_from_slice(field.raw);
    }
    if value != 0 {
        wire::put_varint_field(&mut edited, number, value);
    }
    let mut out = Vec::with_capacity(record.len() + 12);
    for field in &fields {
        if field.number == kind {
            wire::put_len(&mut out, kind, &edited);
        } else {
            out.extend_from_slice(field.raw);
        }
    }
    Ok(Some(out))
}

/// Whether a decoded record is the one for `conversation`.
fn is_chat(record: &StorageRecord, conversation: &ConversationId) -> bool {
    match (&record.record, conversation.as_group()) {
        (Some(Record::GroupV2(group)), Some(key)) => group.master_key == key,
        (Some(Record::Contact(contact)), None) => {
            let aci = conversation.as_direct().and_then(Aci::parse_from_service_id_string);
            aci.is_some_and(|aci| record_aci(contact) == Some(Uuid::from(aci)))
        }
        _ => false,
    }
}

/// A record for a chat Storage Service doesn't know yet, as Desktop makes
/// one when you change a chat without a record.
fn new_chat_record(conversation: &ConversationId) -> Option<(IdentifierType, Vec<u8>)> {
    let (kind, record) = match conversation.as_group() {
        Some(key) => {
            (IdentifierType::Groupv2, Record::GroupV2(GroupV2Record { master_key: key, ..Default::default() }))
        }
        None => {
            let aci = Aci::parse_from_service_id_string(conversation.as_direct()?)?;
            let contact = ContactRecord {
                aci: aci.service_id_string(),
                aci_binary: aci.service_id_binary(),
                ..Default::default()
            };
            (IdentifierType::Contact, Record::Contact(contact))
        }
    };
    Some((kind, StorageRecord { record: Some(record) }.encode_to_vec()))
}

/// Changes one flag on a chat for the whole account.
pub(crate) async fn write_chat(manager: &SignalManager, conversation: &ConversationId, edit: ChatEdit) -> anyhow::Result<()> {
    // The Signal chat has no record of its own (only its pin, in the
    // account record): its state stays on this device.
    if conversation.is_signal() {
        return Ok(());
    }
    let session = Session::new(manager).await?;
    let kind = if conversation.as_group().is_some() { IdentifierType::Groupv2 } else { IdentifierType::Contact };
    for _ in 0..3 {
        let manifest = session.manifest().await?;
        let ikm = manifest_ikm(&manifest);
        let keys: Vec<Vec<u8>> =
            manifest.identifiers.iter().filter(|id| id.r#type() == kind).map(|id| id.raw.clone()).collect();
        let mut existing = None;
        for batch in keys.chunks(READ_BATCH) {
            existing = session.read_many(batch, ikm).await?.into_iter().find(|(_, raw)| {
                StorageRecord::decode(raw.as_slice()).is_ok_and(|record| is_chat(&record, conversation))
            });
            if existing.is_some() {
                break;
            }
        }
        let (old_key, record) = match existing {
            Some((key, raw)) => (Some(key), raw),
            None => {
                let (_, raw) = new_chat_record(conversation).context("this chat can't be saved to Signal's storage")?;
                (None, raw)
            }
        };
        let Some(record) = edit_chat_record(&record, edit)? else { return Ok(()) };
        if session.commit(manager, &manifest, old_key.as_deref(), kind, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
}

fn manifest_ikm(manifest: &ManifestRecord) -> Option<&[u8]> {
    (!manifest.record_ikm.is_empty()).then_some(manifest.record_ikm.as_slice())
}

/// An authenticated Storage Service connection that hands back records as
/// raw bytes (libsignal-service's client only returns decoded ones).
struct Session {
    service: PushService,
    auth: HttpAuth,
    key: StorageServiceKey,
}

impl Session {
    async fn new(manager: &SignalManager) -> anyhow::Result<Self> {
        let master_key = manager
            .store()
            .fetch_master_key()
            .await
            .map_err(|e| anyhow!("{e}"))?
            .context("no master key; relink to use Storage Service")?;
        let service = push_service(manager)?;
        let response = service
            .request(reqwest::Method::GET, Endpoint::service("/v1/storage/auth"), HttpAuthOverride::NoOverride)?
            .send()
            .await?
            .error_for_status()?;
        let auth: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
        let field = |name: &str| auth.get(name).and_then(|v| v.as_str()).map(str::to_owned);
        let auth = HttpAuth {
            username: field("username").context("storage auth has no username")?,
            password: field("password").context("storage auth has no password")?,
        };
        Ok(Self { service, auth, key: StorageServiceKey::from_master_key(&master_key) })
    }

    fn request(&self, method: reqwest::Method, path: &'static str) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(self
            .service
            .request(method, Endpoint::storage(path), HttpAuthOverride::Identified(self.auth.clone()))?
            .header("Content-Type", "application/x-protobuf"))
    }

    async fn manifest(&self) -> anyhow::Result<ManifestRecord> {
        let response = self.request(reqwest::Method::GET, "/v1/storage/manifest")?.send().await?.error_for_status()?;
        let manifest = StorageManifest::decode(response.bytes().await?)?;
        Ok(StorageService::decrypt_manifest(&self.key, &manifest)?)
    }

    /// The decrypted, still-encoded `StorageRecord` stored under `key`.
    async fn read_raw(&self, key: &[u8], ikm: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
        let (_, record) = self
            .read_many(&[key.to_vec()], ikm)
            .await?
            .into_iter()
            .next()
            .context("record missing from storage")?;
        Ok(record)
    }

    /// `(key, decrypted StorageRecord)` for each of `keys` the server has.
    async fn read_many(&self, keys: &[Vec<u8>], ikm: Option<&[u8]>) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let body = ReadOperation { read_key: keys.to_vec() }.encode_to_vec();
        let response = self.request(reqwest::Method::PUT, "/v1/storage/read")?.body(body).send().await?.error_for_status()?;
        let items = StorageItems::decode(response.bytes().await?)?;
        items
            .items
            .into_iter()
            .map(|item| {
                let record = decrypt(&item_key(&self.key, &item.key, ikm), &item.value)?;
                Ok((item.key, record))
            })
            .collect()
    }

    /// Replaces the record under `old_key` (or adds a new one of `kind`) with
    /// `record`, under a new random key, in a new manifest version.
    /// `Ok(false)` if another device wrote first.
    async fn commit(
        &self,
        manager: &SignalManager,
        manifest: &ManifestRecord,
        old_key: Option<&[u8]>,
        kind: IdentifierType,
        record: &[u8],
    ) -> anyhow::Result<bool> {
        let ikm = manifest_ikm(manifest);
        let new_key: [u8; 16] = rand::rng().random();
        let mut next = manifest.clone();
        next.version += 1;
        next.source_device = manager.registration_data().device_id.unwrap_or_default();
        match old_key {
            Some(old_key) => {
                for id in next.identifiers.iter_mut().filter(|id| id.raw == old_key) {
                    id.raw = new_key.to_vec();
                }
            }
            None => next.identifiers.push(Identifier { raw: new_key.to_vec(), r#type: kind as i32 }),
        }
        let item_key = item_key(&self.key, &new_key, ikm);
        let write = WriteOperation {
            manifest: Some(StorageService::encrypt_manifest(&self.key, &next)),
            insert_item: vec![StorageItem { key: new_key.to_vec(), value: encrypt(&item_key, record)? }],
            delete_key: old_key.map(|k| vec![k.to_vec()]).unwrap_or_default(),
            clear_all: false,
        };
        self.write(&write).await
    }

    /// `Ok(false)` on a version conflict.
    async fn write(&self, write: &WriteOperation) -> anyhow::Result<bool> {
        let response = self.request(reqwest::Method::PUT, "/v1/storage")?.body(write.encode_to_vec()).send().await?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            return Ok(false);
        }
        response.error_for_status()?;
        Ok(true)
    }
}

// Same derivations as libsignal-service's `StorageService` (private there).
const IV_LEN: usize = 12;
const ITEM_KEY_INFO_PREFIX: &[u8] = b"20240801_SIGNAL_STORAGE_SERVICE_ITEM_";

/// Modern accounts derive item keys from the manifest's `record_ikm`; older
/// ones from the storage key.
fn item_key(storage_key: &StorageServiceKey, raw_id: &[u8], ikm: Option<&[u8]>) -> [u8; 32] {
    let mut key = [0u8; 32];
    match ikm {
        Some(ikm) => Hkdf::<Sha256>::new(None, ikm)
            .expand_multi_info(&[ITEM_KEY_INFO_PREFIX, raw_id], &mut key)
            .expect("32 bytes is a valid HKDF output length"),
        None => {
            let mut mac = Hmac::<Sha256>::new_from_slice(&storage_key.inner).expect("any key length");
            mac.update(b"Item_");
            mac.update(base64::engine::general_purpose::STANDARD.encode(raw_id).as_bytes());
            key = mac.finalize().into_bytes().into();
        }
    }
    key
}

/// AES-256-GCM, `iv(12) || ciphertext || tag`.
fn decrypt(key: &[u8; 32], blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (iv, ciphertext) = blob.split_first_chunk::<IV_LEN>().context("storage item too short")?;
    Aes256Gcm::new(key.into()).decrypt(iv.into(), ciphertext).map_err(|_| anyhow!("can't decrypt storage item"))
}

fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let iv: [u8; IV_LEN] = rand::rng().random();
    let ciphertext =
        Aes256Gcm::new(key.into()).encrypt((&iv).into(), plaintext).map_err(|_| anyhow!("can't encrypt storage item"))?;
    Ok([&iv[..], &ciphertext].concat())
}

/// presage keeps its authenticated push service private, so build our own
/// from the registration data. The account password is `pub(crate)` there,
/// but it is part of the data's serialized form (that is how presage stores
/// it), so read it back from that.
pub(crate) fn push_service(manager: &SignalManager) -> anyhow::Result<PushService> {
    let data = manager.registration_data();
    let serialized = serde_json::to_value(data)?;
    let password = serialized
        .get("password")
        .and_then(|p| p.as_str())
        .context("registration data has no password")?
        .to_owned();
    let credentials = ServiceCredentials {
        aci: Some(data.service_ids.aci),
        pni: Some(data.service_ids.pni),
        phonenumber: data.phone_number.to_string().parse::<E164>().map_err(|e| anyhow!("{e:?}"))?,
        password: Some(password),
        device_id: data.device_id.and_then(|d| d.try_into().ok()),
    };
    Ok(PushService::new(data.signal_servers, Some(credentials), "signal-gpui"))
}

/// A story list (Desktop's `StoryDistributionListRecord`): its members as
/// service ids (the binary form preferred), unless it's been deleted.
fn story_list(record: &presage::proto::StoryDistributionListRecord) -> Option<crate::StoryList> {
    if record.deleted_at_timestamp != 0 {
        return None;
    }
    let id = Uuid::from_slice(&record.identifier).ok()?;
    let members = if record.recipient_service_ids_binary.is_empty() {
        record.recipient_service_ids.iter().filter_map(|s| ServiceId::parse_from_service_id_string(s)).collect::<Vec<_>>()
    } else {
        record.recipient_service_ids_binary.iter().filter_map(|b| ServiceId::parse_from_service_id_binary(b)).collect()
    };
    Some(crate::StoryList {
        id: id.to_string(),
        name: record.name.clone(),
        members: members.iter().map(ServiceId::service_id_string).collect(),
        block_list: record.is_block_list,
        allows_replies: record.allows_replies,
    })
}

fn contact_chat(record: &ContactRecord) -> Option<ChatState> {
    let aci = record_aci(record)?;
    Some(ChatState {
        id: ConversationId::direct(&Aci::from(aci).service_id_string()),
        archived: record.archived,
        muted_until: clamp_millis(record.muted_until_timestamp),
        marked_unread: record.marked_unread,
        blocked: record.blocked,
    })
}

fn group_chat(record: &GroupV2Record) -> Option<ChatState> {
    (record.master_key.len() == 32).then(|| ChatState {
        id: ConversationId::group(&record.master_key),
        archived: record.archived,
        muted_until: clamp_millis(record.muted_until_timestamp),
        marked_unread: record.marked_unread,
        blocked: record.blocked,
    })
}

/// Signal uses `u64::MAX` for "muted forever".
fn clamp_millis(millis: u64) -> i64 {
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn record_aci(record: &ContactRecord) -> Option<Uuid> {
    let aci = if record.aci_binary.len() == 16 {
        Uuid::from_slice(&record.aci_binary).ok()?
    } else {
        record.aci.parse().ok()?
    };
    (!aci.is_nil()).then_some(aci)
}

fn join_name(given: &str, family: &str) -> Option<String> {
    let name = format!("{} {}", given.trim(), family.trim());
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

pub(crate) fn from_record(record: &ContactRecord) -> Option<StorageContact> {
    let aci = record_aci(record)?;
    let nickname = record.nickname.as_ref().and_then(|n| join_name(&n.given, &n.family));
    let system = Some(record.system_nickname.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| join_name(&record.system_given_name, &record.system_family_name));
    let profile = join_name(&record.given_name, &record.family_name);
    Some(StorageContact {
        aci,
        name: nickname.or(system).or(profile),
        profile_key: record.profile_key.as_slice().try_into().ok(),
        e164: Some(record.e164.clone()).filter(|e| !e.is_empty()),
        hidden: record.hidden || record.blocked,
    })
}

#[cfg(test)]
mod tests {
    use presage::proto::contact_record::Name;

    use super::*;

    const ACI: &str = "6b1a2b3c-0000-4000-8000-000000000001";

    #[test]
    fn name_precedence_matches_signal() {
        let mut record = ContactRecord {
            aci: ACI.into(),
            given_name: "Jonathan".into(),
            family_name: "Doe".into(),
            profile_key: vec![7; 32],
            ..Default::default()
        };
        let c = from_record(&record).unwrap();
        assert_eq!(c.name.as_deref(), Some("Jonathan Doe"));
        assert_eq!(c.profile_key, Some([7; 32]));

        record.system_given_name = "Jon".into();
        assert_eq!(from_record(&record).unwrap().name.as_deref(), Some("Jon"));

        record.nickname = Some(Name { given: "J".into(), family: String::new() });
        assert_eq!(from_record(&record).unwrap().name.as_deref(), Some("J"));
    }

    #[test]
    fn pinned_keeps_order_and_skips_unknown_kinds() {
        use presage::proto::account_record::{PinnedConversation, pinned_conversation::Contact};
        let pin = |identifier| PinnedConversation { identifier: Some(identifier) };
        let account = AccountRecord {
            pinned_conversations: vec![
                pin(PinnedId::GroupMasterKey(vec![0xab; 32])),
                pin(PinnedId::LegacyGroupId(vec![1; 16])),
                pin(PinnedId::Contact(Contact { service_id: ACI.into(), ..Default::default() })),
                pin(PinnedId::Contact(Contact { e164: "+15550100".into(), ..Default::default() })),
            ],
            ..Default::default()
        };
        assert_eq!(
            pinned(&account),
            [ConversationId::group(&[0xab; 32]), ConversationId::direct(ACI)]
        );
    }

    fn account_record(account: &AccountRecord) -> Vec<u8> {
        presage::proto::StorageRecord { record: Some(Record::Account(account.clone())) }.encode_to_vec()
    }

    fn decode_account(record: &[u8]) -> AccountRecord {
        match presage::proto::StorageRecord::decode(record).unwrap().record {
            Some(Record::Account(account)) => account,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pin_edits_keep_unknown_fields_and_entries() {
        use presage::proto::account_record::PinnedConversation;
        let group = ConversationId::group(&[0xab; 32]);
        let jon = ConversationId::direct(ACI);
        let legacy = PinnedConversation { identifier: Some(PinnedId::LegacyGroupId(vec![1; 16])) };
        let account = AccountRecord {
            given_name: "Ken".into(),
            pinned_conversations: vec![pin_for(&group).unwrap(), legacy.clone()],
            ..Default::default()
        };
        let mut record = account_record(&account);
        // Fields this protobuf version doesn't know: one inside the account
        // record, one beside it.
        let mut account_bytes = account.encode_to_vec();
        wire::put_len(&mut account_bytes, 999, b"future account field");
        record.clear();
        wire::put_len(&mut record, RECORD_ACCOUNT, &account_bytes);
        wire::put_len(&mut record, 998, b"future record field");

        let pinned_record = edit_pins(&record, &PinEdit::Pin(jon.clone())).unwrap().unwrap();
        let account = decode_account(&pinned_record);
        assert_eq!(account.given_name, "Ken");
        assert_eq!(account.pinned_conversations, [pin_for(&group).unwrap(), legacy.clone(), pin_for(&jon).unwrap()]);
        assert_eq!(pinned(&account), [group.clone(), jon.clone()]);
        let contains = |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
        assert!(contains(&pinned_record, b"future account field"));
        assert!(contains(&pinned_record, b"future record field"));

        // No-ops.
        assert_eq!(edit_pins(&pinned_record, &PinEdit::Pin(jon.clone())).unwrap(), None);
        assert_eq!(edit_pins(&record, &PinEdit::Unpin(jon.clone())).unwrap(), None);

        let unpinned = edit_pins(&pinned_record, &PinEdit::Unpin(group.clone())).unwrap().unwrap();
        let account = decode_account(&unpinned);
        assert_eq!(account.pinned_conversations, [legacy, pin_for(&jon).unwrap()]);
        assert!(contains(&unpinned, b"future account field"));
    }

    #[test]
    fn the_signal_chat_pins_as_release_notes() {
        let pin = pin_for(&ConversationId::signal()).unwrap();
        assert!(matches!(pin.identifier, Some(PinnedId::ReleaseNotes(_))), "as Desktop pins it");
        assert_eq!(pinned_id(&pin), Some(ConversationId::signal()));
    }

    #[test]
    fn pin_limit_counts_every_entry() {
        let pins = (0..MAX_PINNED as u8).map(|i| pin_for(&ConversationId::group(&[i; 32])).unwrap()).collect();
        let record = account_record(&AccountRecord { pinned_conversations: pins, ..Default::default() });
        let error = edit_pins(&record, &PinEdit::Pin(ConversationId::direct(ACI))).unwrap_err();
        assert!(error.to_string().contains("up to 4"), "{error}");
        // Unpinning still works at the limit.
        assert!(edit_pins(&record, &PinEdit::Unpin(ConversationId::group(&[0; 32]))).unwrap().is_some());
    }

    #[test]
    fn edit_rejects_non_account_records() {
        let record = presage::proto::StorageRecord {
            record: Some(Record::Contact(ContactRecord { aci: ACI.into(), ..Default::default() })),
        }
        .encode_to_vec();
        assert!(edit_pins(&record, &PinEdit::Pin(ConversationId::direct(ACI))).is_err());
    }

    #[test]
    fn item_crypto_matches_libsignal_service() {
        let storage_key = StorageServiceKey { inner: [3; 32] };
        let record = presage::proto::StorageRecord {
            record: Some(Record::Account(AccountRecord { given_name: "Ken".into(), ..Default::default() })),
        };
        for ikm in [Some(&[9u8; 32][..]), None] {
            let raw_id = vec![5u8; 16];
            // Theirs encrypts, ours decrypts.
            let item = StorageService::encrypt_item(&storage_key, raw_id.clone(), &record, ikm);
            assert_eq!(decrypt(&item_key(&storage_key, &raw_id, ikm), &item.value).unwrap(), record.encode_to_vec());
            // Ours encrypts, theirs decrypts.
            let value = encrypt(&item_key(&storage_key, &raw_id, ikm), &record.encode_to_vec()).unwrap();
            let decoded = StorageService::decrypt_item(&storage_key, &StorageItem { key: raw_id, value }, ikm).unwrap();
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn chat_edits_change_one_field_and_keep_the_rest() {
        let contact = ContactRecord { aci: ACI.into(), archived: true, ..Default::default() };
        let mut record = StorageRecord { record: Some(Record::Contact(contact)) }.encode_to_vec();
        // A field this client doesn't know (contact field 99, varint 7) must survive.
        let fields = wire::fields(&record).unwrap();
        let mut inner = fields[0].payload.unwrap().to_vec();
        wire::put_varint_field(&mut inner, 99, 7);
        record.clear();
        wire::put_len(&mut record, RECORD_CONTACT, &inner);

        let muted = edit_chat_record(&record, ChatEdit::MutedUntil(i64::MAX as u64)).unwrap().unwrap();
        let decoded = StorageRecord::decode(muted.as_slice()).unwrap();
        let Some(Record::Contact(c)) = decoded.record else { panic!() };
        assert!(c.archived);
        assert_eq!(c.muted_until_timestamp, i64::MAX as u64);
        let inner = wire::fields(&muted).unwrap()[0].payload.unwrap().to_vec();
        assert!(wire::fields(&inner).unwrap().iter().any(|f| f.number == 99 && f.varint == Some(7)));

        // Clearing removes the field; setting what's there changes nothing.
        let unarchived = edit_chat_record(&muted, ChatEdit::Archived(false)).unwrap().unwrap();
        let Some(Record::Contact(c)) = StorageRecord::decode(unarchived.as_slice()).unwrap().record else { panic!() };
        assert!(!c.archived);
        assert_eq!(edit_chat_record(&unarchived, ChatEdit::Archived(false)).unwrap(), None);
    }

    #[test]
    fn hiding_stories_writes_hide_story() {
        for (record, group) in [
            (StorageRecord { record: Some(Record::Contact(ContactRecord { aci: ACI.into(), ..Default::default() })) }, false),
            (StorageRecord { record: Some(Record::GroupV2(GroupV2Record { master_key: vec![1; 32], ..Default::default() })) }, true),
        ] {
            let hidden = edit_chat_record(&record.encode_to_vec(), ChatEdit::HideStory(true)).unwrap().unwrap();
            let hide_story = match StorageRecord::decode(hidden.as_slice()).unwrap().record {
                Some(Record::Contact(c)) => c.hide_story,
                Some(Record::GroupV2(g)) => g.hide_story,
                _ => panic!(),
            };
            assert!(hide_story, "group: {group}");
        }
    }

    #[test]
    fn story_lists_read_as_desktop_reads_them() {
        use presage::proto::StoryDistributionListRecord;
        let my_story = StoryDistributionListRecord {
            identifier: vec![0; 16],
            name: String::new(),
            recipient_service_ids: vec!["ignored-when-binary-is-there".into()],
            recipient_service_ids_binary: vec![Uuid::parse_str(ACI).unwrap().as_bytes().to_vec()],
            is_block_list: true,
            allows_replies: true,
            ..Default::default()
        };
        let list = story_list(&my_story).unwrap();
        assert_eq!(
            (list.id.as_str(), list.members, list.block_list, list.allows_replies),
            (crate::MY_STORY_ID, vec![ACI.to_owned()], true, true)
        );
        let deleted = StoryDistributionListRecord { identifier: vec![1; 16], deleted_at_timestamp: 5, ..Default::default() };
        assert_eq!(story_list(&deleted), None);
    }

    #[test]
    fn new_chat_records_match_their_chat() {
        let direct = ConversationId::direct(ACI);
        let (kind, raw) = new_chat_record(&direct).unwrap();
        assert_eq!(kind, IdentifierType::Contact);
        let record = StorageRecord::decode(raw.as_slice()).unwrap();
        assert!(is_chat(&record, &direct));
        assert!(!is_chat(&record, &ConversationId::group(&[1; 32])));

        let group = ConversationId::group(&[1; 32]);
        let raw = edit_chat_record(&new_chat_record(&group).unwrap().1, ChatEdit::Blocked(true)).unwrap().unwrap();
        let record = StorageRecord::decode(raw.as_slice()).unwrap();
        assert!(is_chat(&record, &group));
        let Some(Record::GroupV2(g)) = record.record else { panic!() };
        assert!(g.blocked);
    }

    #[test]
    fn chat_states_from_records() {
        let contact = ContactRecord { aci: ACI.into(), archived: true, muted_until_timestamp: u64::MAX, ..Default::default() };
        assert_eq!(
            contact_chat(&contact),
            Some(ChatState {
                id: ConversationId::direct(ACI),
                archived: true,
                muted_until: i64::MAX,
                marked_unread: false,
                blocked: false
            })
        );
        let group = GroupV2Record { master_key: vec![1; 32], muted_until_timestamp: 5, ..Default::default() };
        assert_eq!(
            group_chat(&group),
            Some(ChatState {
                id: ConversationId::group(&[1; 32]),
                archived: false,
                muted_until: 5,
                marked_unread: false,
                blocked: false
            })
        );
        assert_eq!(group_chat(&GroupV2Record { master_key: vec![1; 16], ..Default::default() }), None);
    }

    #[test]
    fn skips_records_without_an_aci() {
        assert_eq!(from_record(&ContactRecord { e164: "+15550100".into(), ..Default::default() }), None);
        let c = from_record(&ContactRecord { aci: ACI.into(), profile_key: vec![1; 5], ..Default::default() }).unwrap();
        assert_eq!((c.name, c.profile_key), (None, None));
    }
}
