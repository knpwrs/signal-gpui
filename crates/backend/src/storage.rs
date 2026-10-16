//! Storage Service: the account's encrypted state (contacts, groups, account
//! settings), kept by the primary device. Signal Desktop takes most display
//! names, profile keys, pins and archived/muted state from it; contact sync
//! only carries names from the phone's address book.
//!
//! Pins, per-chat flags (archived, muted, marked unread, blocked, hidden
//! stories), the account's story settings and story lists are also
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
    /// The nickname and note we gave them, and the name without the
    /// nickname.
    pub nickname: signal_store::ContactNickname,
    /// The record has a name from the phone's address book (Desktop's
    /// `isInSystemContacts`: `systemGivenName` and the rest).
    pub system_name: bool,
    /// Their username (Desktop's `contactRecord.username`).
    pub username: Option<String>,
}

/// What we read from the Storage Service.
#[derive(Debug, Default)]
pub(crate) struct StorageSnapshot {
    pub contacts: Vec<StorageContact>,
    /// Pinned conversations in display order; `None` without an account record.
    pub pinned: Option<Vec<ConversationId>>,
    /// Archived/muted state of direct and group chats.
    pub chats: Vec<ChatState>,
    /// The account's privacy settings (read receipts, typing indicators,
    /// link previews, sealed sender indicators); unset without an account
    /// record.
    pub settings: crate::AccountSettings,
    /// Story lists, My Story among them (not deleted ones).
    pub story_lists: Vec<crate::StoryList>,
    /// Chats whose stories are hidden (`hideStory`).
    pub hidden_stories: Vec<ConversationId>,
    /// The account's `storiesDisabled`.
    pub stories_disabled: Option<bool>,
    /// The account's `storyViewReceiptsEnabled`, when set.
    pub story_view_receipts: Option<bool>,
    /// The account's `hasSetMyStoriesPrivacy`.
    pub my_story_privacy_set: Option<bool>,
    /// Call links we made (on any of our devices), deleted ones too.
    pub call_links: Vec<StorageCallLink>,
    /// Whether we share our profile with each contact and group that has a
    /// record (`whitelisted`, Desktop's `profileSharing`).
    pub profile_sharing: Vec<(ConversationId, bool)>,
    /// Each group's story send mode (`storySendMode`).
    pub story_send_modes: Vec<(ConversationId, signal_store::StorySendMode)>,
    /// The account's `backupTier` (a zkgroup `BackupLevel`), `Some(0)` when
    /// backups are off; `None` without an account record.
    pub backup_tier: Option<u64>,
    /// What key transparency needs from the account record.
    pub identifiers: Option<AccountIdentifiers>,
    /// Our profile as the account record has it.
    pub own_profile: Option<OwnProfileRecord>,
    /// Chat folders, deleted ones too.
    pub chat_folders: Vec<signal_store::ChatFolder>,
}

/// What the account record says about our profile and username (Desktop's
/// `mergeAccountRecord`): our name, our photo's CDN path, our profile key
/// (the primary's, which a linked device follows), our username link and
/// whether our number is shared with everybody.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct OwnProfileRecord {
    pub given_name: String,
    pub family_name: String,
    pub avatar_url_path: Option<String>,
    pub profile_key: Option<[u8; 32]>,
    pub username_link: Option<crate::usernames::UsernameLink>,
    /// Desktop's `phoneNumberSharingMode` merge: unset or EVERYBODY share,
    /// UNKNOWN and NOBODY don't. proto3 can't tell unset from UNKNOWN (0),
    /// so 0 shares, as an unset field would.
    pub sharing_number: bool,
}

impl OwnProfileRecord {
    pub(crate) fn from_record(account: &AccountRecord) -> Self {
        use presage::proto::account_record::PhoneNumberSharingMode;
        let link = account.username_link.as_ref().filter(|l| !l.entropy.is_empty() && !l.server_id.is_empty());
        Self {
            given_name: account.given_name.trim().to_owned(),
            family_name: account.family_name.trim().to_owned(),
            avatar_url_path: Some(account.avatar_url_path.clone()).filter(|p| !p.is_empty()),
            profile_key: account.profile_key.as_slice().try_into().ok(),
            username_link: link.map(|l| crate::usernames::UsernameLink {
                entropy: l.entropy.clone(),
                server_id: l.server_id.clone(),
                // Desktop: a color it doesn't know is 0.
                color: if (0..=8).contains(&l.color) { l.color } else { 0 },
            }),
            sharing_number: account.phone_number_sharing_mode != PhoneNumberSharingMode::Nobody as i32,
        }
    }
}

/// A call link record (Desktop's `mergeCallLinkRecord`): only links we're
/// admin of are kept there; a deleted one keeps its root key and when.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StorageCallLink {
    pub root_key: Vec<u8>,
    pub admin_key: Option<Vec<u8>>,
    /// Unix millis; a record with both this and an admin key is deleted
    /// (Desktop: "Assuming deleted").
    pub deleted_at: Option<i64>,
}

fn call_link(record: &presage::proto::CallLinkRecord) -> Option<StorageCallLink> {
    if record.root_key.is_empty() {
        return None;
    }
    Some(StorageCallLink {
        root_key: record.root_key.clone(),
        admin_key: (!record.admin_passkey.is_empty()).then(|| record.admin_passkey.clone()),
        deleted_at: (record.deleted_at_timestamp_ms != 0).then(|| clamp_millis(record.deleted_at_timestamp_ms)),
    })
}

/// The account record's fields key transparency depends on.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct AccountIdentifiers {
    /// `username`, when set.
    pub username: Option<String>,
    /// `unlistedPhoneNumber`: our number isn't discoverable.
    pub unlisted_phone_number: bool,
    /// `automaticKeyVerificationDisabled`.
    pub key_transparency_disabled: bool,
    /// The record carries our profile key.
    pub has_profile_key: bool,
}

impl AccountIdentifiers {
    pub(crate) fn from_record(account: &AccountRecord) -> Self {
        Self {
            username: Some(account.username.clone()).filter(|u| !u.is_empty()),
            unlisted_phone_number: account.unlisted_phone_number,
            key_transparency_disabled: account.automatic_key_verification_disabled,
            has_profile_key: !account.profile_key.is_empty(),
        }
    }
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
                    | IdentifierType::CallLink
                    | IdentifierType::ChatFolder
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
                    if let Some(aci) = record_aci(&contact) {
                        let chat = ConversationId::direct(&Aci::from(aci).service_id_string());
                        if contact.hide_story {
                            snapshot.hidden_stories.push(chat.clone());
                        }
                        snapshot.profile_sharing.push((chat, contact.whitelisted));
                    }
                    snapshot.contacts.extend(from_record(&contact));
                }
                Some(Record::GroupV2(group)) => {
                    if group.master_key.len() == 32 {
                        let chat = ConversationId::group(&group.master_key);
                        if group.hide_story {
                            snapshot.hidden_stories.push(chat.clone());
                        }
                        snapshot.profile_sharing.push((chat.clone(), group.whitelisted));
                        let mode = signal_store::StorySendMode::from_wire(i64::from(group.story_send_mode));
                        snapshot.story_send_modes.push((chat, mode));
                    }
                    snapshot.chats.extend(group_chat(&group));
                }
                Some(Record::Account(account)) => {
                    snapshot.pinned = Some(pinned(&account));
                    snapshot.settings = account_settings(&account);
                    snapshot.identifiers = Some(AccountIdentifiers::from_record(&account));
                    snapshot.own_profile = Some(OwnProfileRecord::from_record(&account));
                    snapshot.stories_disabled = Some(account.stories_disabled);
                    snapshot.my_story_privacy_set = Some(account.has_set_my_stories_privacy);
                    // Desktop's `saveBackupTier(toNumber(backupTier))`:
                    // unset means backups are off.
                    snapshot.backup_tier = Some(account.backup_tier.unwrap_or(0));
                    snapshot.story_view_receipts = match account.story_view_receipts_enabled() {
                        presage::proto::OptionalBool::Enabled => Some(true),
                        presage::proto::OptionalBool::Disabled => Some(false),
                        presage::proto::OptionalBool::Unset => None,
                    };
                }
                Some(Record::StoryDistributionList(list)) => snapshot.story_lists.extend(story_list(&list)),
                Some(Record::CallLink(link)) => snapshot.call_links.extend(call_link(&link)),
                Some(Record::ChatFolder(folder)) => snapshot.chat_folders.extend(chat_folder(&folder)),
                _ => {}
            }
        }
    }
    Ok(snapshot)
}

/// The account record's settings, as Desktop's `mergeAccountRecord`
/// (`ts/services/storageRecordOps.preload.ts`) takes them: every one; a
/// plain flag the record leaves out is off, an unset `OptionalBool` is the
/// setting's default, and an unset sharing mode is everybody.
pub(crate) fn account_settings(account: &AccountRecord) -> crate::AccountSettings {
    use crate::AccountSetting as S;
    use presage::proto::OptionalBool;
    let optional = |setting: S, value: OptionalBool| match value {
        OptionalBool::Enabled => true,
        OptionalBool::Disabled => false,
        OptionalBool::Unset => setting.default_on(),
    };
    let flags = [
        (S::ReadReceipts, account.read_receipts),
        (S::TypingIndicators, account.typing_indicators),
        (S::LinkPreviews, account.link_previews),
        (S::SealedSenderIndicators, account.sealed_sender_indicators),
        (S::PreferContactAvatars, account.prefer_contact_avatars),
        (S::KeepMutedChatsArchived, account.keep_muted_chats_archived),
        (S::ReactionNotifications, optional(S::ReactionNotifications, account.reaction_notifications())),
        (S::NotifyCallsIfMuted, optional(S::NotifyCallsIfMuted, account.notify_for_calls_if_muted())),
        (S::NotifyMentionsIfMuted, optional(S::NotifyMentionsIfMuted, account.notify_for_mentions_if_muted())),
        (S::NotifyRepliesIfMuted, optional(S::NotifyRepliesIfMuted, account.notify_for_replies_if_muted())),
        (S::ShowUnreadReminders, optional(S::ShowUnreadReminders, account.show_unread_reminders())),
    ];
    // `PhoneNumberSharingMode`: unset (0 is never written) and EVERYBODY
    // are everybody, NOBODY nobody.
    let sharing = account.phone_number_sharing_mode != presage::proto::account_record::PhoneNumberSharingMode::Nobody as i32;
    crate::AccountSettings {
        flags: flags.into_iter().collect(),
        universal_expire_timer: Some(account.universal_expire_timer),
        phone_number_sharing: Some(sharing),
    }
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
    edit_inner(record, RECORD_ACCOUNT, |account| edit_account_pins(account, edit))
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
    // Desktop's `getPinnedConversationLimit`, from the remote config.
    let limit = crate::remote_config().pinned_chat_limit();
    ensure!(!pinning || pins < limit, "you can pin up to {limit} chats");

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
    /// We share our profile with them (`whitelisted`).
    ProfileSharing(bool),
    /// A group's story send mode (`storySendMode`); contact records have
    /// none.
    StorySendMode(signal_store::StorySendMode),
}

impl ChatEdit {
    /// The field number in a `ContactRecord` or `GroupV2Record`, and its value.
    /// `None` for a field the record doesn't have.
    fn field(self, group: bool) -> Option<(u32, u64)> {
        let (contact, group_field, value) = match self {
            ChatEdit::Blocked(on) => (Some(9), 2, u64::from(on)),
            ChatEdit::Archived(on) => (Some(11), 4, u64::from(on)),
            ChatEdit::MarkedUnread(on) => (Some(12), 5, u64::from(on)),
            ChatEdit::MutedUntil(until) => (Some(13), 6, until),
            ChatEdit::HideStory(on) => (Some(14), 8, u64::from(on)),
            ChatEdit::ProfileSharing(on) => (Some(10), 3, u64::from(on)),
            ChatEdit::StorySendMode(mode) => (None, 10, mode.to_wire() as u64),
        };
        Some((if group { group_field } else { contact? }, value))
    }
}

/// Applies `edit` to an encoded `StorageRecord` holding a contact or group
/// record, changing only that one field. `Ok(None)` when it's already set.
pub(crate) fn edit_chat_record(record: &[u8], edit: ChatEdit) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(record).context("malformed storage record")?;
    let kind = fields
        .iter()
        .find(|f| f.payload.is_some() && matches!(f.number, RECORD_CONTACT | RECORD_GROUP_V2))
        .map(|f| f.number)
        .context("storage record is not a contact or group")?;
    let (number, value) = edit.field(kind == RECORD_GROUP_V2).context("a contact record has no such field")?;
    edit_inner(record, kind, |chat| set_varint(chat, number, value))
}

/// Rewrites the record held in field `kind` of an encoded `StorageRecord`
/// with `edit`, copying every other byte. `Ok(None)` when `edit` changes
/// nothing.
fn edit_inner(
    record: &[u8],
    kind: u32,
    edit: impl FnOnce(&[u8]) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(record).context("malformed storage record")?;
    let inner = fields
        .iter()
        .find(|f| f.number == kind)
        .and_then(|f| f.payload)
        .context("storage record holds another kind of record")?;
    let Some(edited) = edit(inner)? else { return Ok(None) };
    let mut out = Vec::with_capacity(record.len() + edited.len());
    for field in &fields {
        if field.number == kind {
            wire::put_len(&mut out, kind, &edited);
        } else {
            out.extend_from_slice(field.raw);
        }
    }
    Ok(Some(out))
}

/// Sets varint field `number` of an encoded message to `value` (0 leaves
/// it out, as proto3 writes it). `Ok(None)` when it's already that.
fn set_varint(message: &[u8], number: u32, value: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(message).context("malformed record")?;
    // proto3: the last occurrence wins, and absent means zero.
    let current = fields.iter().rev().find(|f| f.number == number).and_then(|f| f.varint).unwrap_or(0);
    if current == value {
        return Ok(None);
    }
    let mut edited = Vec::with_capacity(message.len() + 12);
    for field in fields.iter().filter(|f| f.number != number) {
        edited.extend_from_slice(field.raw);
    }
    if value != 0 {
        wire::put_varint_field(&mut edited, number, value);
    }
    Ok(Some(edited))
}

/// A change to one of the account record's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AccountEdit {
    /// One of the on/off settings (`readReceipts`, `keepMutedChatsArchived`,
    /// `reactionNotifications`…).
    Setting(crate::AccountSetting, bool),
    /// `universalExpireTimer`, in seconds.
    UniversalExpireTimer(u32),
    /// `phoneNumberSharingMode`: EVERYBODY (`true`) or NOBODY.
    PhoneNumberSharing(bool),
    /// `unlistedPhoneNumber`.
    UnlistedPhoneNumber(bool),
    /// `storiesDisabled` (Desktop's `hasStoriesDisabled`).
    StoriesDisabled(bool),
    /// `storyViewReceiptsEnabled`, an `OptionalBool`.
    StoryViewReceipts(bool),
    /// `hasSetMyStoriesPrivacy`: My Story's viewers were chosen.
    HasSetMyStoriesPrivacy(bool),
    /// `automaticKeyVerificationDisabled` (Desktop's
    /// `hasKeyTransparencyDisabled`).
    AutomaticKeyVerificationDisabled(bool),
}

impl AccountEdit {
    /// The `AccountRecord` field and its value.
    fn field(self) -> (u32, u64) {
        use crate::AccountSetting;
        // OptionalBool: ENABLED = 1, DISABLED = 2.
        let optional = |on: bool| if on { 1 } else { 2 };
        match self {
            AccountEdit::Setting(setting, on) => match setting {
                AccountSetting::ReadReceipts => (6, u64::from(on)),
                AccountSetting::SealedSenderIndicators => (7, u64::from(on)),
                AccountSetting::TypingIndicators => (8, u64::from(on)),
                AccountSetting::LinkPreviews => (11, u64::from(on)),
                AccountSetting::PreferContactAvatars => (15, u64::from(on)),
                AccountSetting::KeepMutedChatsArchived => (25, u64::from(on)),
                // Desktop's protos/SignalStorage.proto.
                AccountSetting::ReactionNotifications => (55, optional(on)),
                AccountSetting::NotifyCallsIfMuted => (56, optional(on)),
                AccountSetting::NotifyMentionsIfMuted => (57, optional(on)),
                AccountSetting::NotifyRepliesIfMuted => (58, optional(on)),
                AccountSetting::ShowUnreadReminders => (59, optional(on)),
            },
            AccountEdit::UniversalExpireTimer(seconds) => (17, u64::from(seconds)),
            AccountEdit::PhoneNumberSharing(everybody) => (12, if everybody { 1 } else { 2 }),
            AccountEdit::UnlistedPhoneNumber(on) => (13, u64::from(on)),
            AccountEdit::HasSetMyStoriesPrivacy(on) => (26, u64::from(on)),
            AccountEdit::StoriesDisabled(on) => (29, u64::from(on)),
            // OptionalBool: ENABLED = 1, DISABLED = 2.
            AccountEdit::StoryViewReceipts(on) => (30, if on { 1 } else { 2 }),
            AccountEdit::AutomaticKeyVerificationDisabled(on) => (46, u64::from(on)),
        }
    }
}

/// Applies `edits` to an encoded `StorageRecord` holding the account
/// record, changing only those fields. `Ok(None)` when they're all set.
pub(crate) fn edit_account(record: &[u8], edits: &[AccountEdit]) -> anyhow::Result<Option<Vec<u8>>> {
    edit_inner(record, RECORD_ACCOUNT, |account| {
        let mut current = account.to_vec();
        let mut changed = false;
        for edit in edits {
            let (number, value) = edit.field();
            if let Some(edited) = set_varint(&current, number, value)? {
                current = edited;
                changed = true;
            }
        }
        Ok(changed.then_some(current))
    })
}

/// Changes settings in the account record, for the whole account.
pub(crate) async fn write_account(manager: &SignalManager, edits: &[AccountEdit]) -> anyhow::Result<()> {
    write_account_with(manager, |record| edit_account(record, edits)).await
}

/// A change to one of the account record's string or message fields.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AccountFieldEdit {
    /// `givenName` and `familyName` (Desktop's `toAccountRecord` writes
    /// our profile name there).
    Name { given: String, family: String },
    /// `avatarUrlPath`: our photo's CDN path (`None`: no photo).
    AvatarPath(Option<String>),
    /// `username`.
    Username(Option<String>),
    /// `usernameLink`.
    UsernameLink(Option<crate::usernames::UsernameLink>),
}

const ACCOUNT_GIVEN_NAME: u32 = 2;
const ACCOUNT_FAMILY_NAME: u32 = 3;
const ACCOUNT_AVATAR_URL_PATH: u32 = 4;
const ACCOUNT_USERNAME: u32 = 33;
const ACCOUNT_USERNAME_LINK: u32 = 35;

impl AccountFieldEdit {
    /// The fields it sets and their encoded values (empty: left out).
    fn fields(&self) -> Vec<(u32, Vec<u8>)> {
        match self {
            Self::Name { given, family } => {
                vec![(ACCOUNT_GIVEN_NAME, given.as_bytes().to_vec()), (ACCOUNT_FAMILY_NAME, family.as_bytes().to_vec())]
            }
            Self::AvatarPath(path) => vec![(ACCOUNT_AVATAR_URL_PATH, path.clone().unwrap_or_default().into_bytes())],
            Self::Username(username) => vec![(ACCOUNT_USERNAME, username.clone().unwrap_or_default().into_bytes())],
            Self::UsernameLink(link) => {
                let encoded = link
                    .as_ref()
                    .map(|l| {
                        presage::proto::account_record::UsernameLink {
                            entropy: l.entropy.clone(),
                            server_id: l.server_id.clone(),
                            color: l.color,
                        }
                        .encode_to_vec()
                    })
                    .unwrap_or_default();
                vec![(ACCOUNT_USERNAME_LINK, encoded)]
            }
        }
    }
}

/// Sets length-delimited field `number` of an encoded message to `value`
/// (empty leaves it out, as proto3 writes it), keeping every other byte.
/// `Ok(None)` when it's already that.
fn set_bytes(message: &[u8], number: u32, value: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let fields = wire::fields(message).context("malformed record")?;
    let current = fields.iter().rev().find(|f| f.number == number).and_then(|f| f.payload).unwrap_or_default();
    if current == value {
        return Ok(None);
    }
    let mut edited = Vec::with_capacity(message.len() + value.len() + 4);
    for field in fields.iter().filter(|f| f.number != number) {
        edited.extend_from_slice(field.raw);
    }
    if !value.is_empty() {
        wire::put_len(&mut edited, number, value);
    }
    Ok(Some(edited))
}

/// Applies `edits` to an encoded `StorageRecord` holding the account
/// record, changing only those fields. `Ok(None)` when they're all set.
pub(crate) fn edit_account_fields(record: &[u8], edits: &[AccountFieldEdit]) -> anyhow::Result<Option<Vec<u8>>> {
    edit_inner(record, RECORD_ACCOUNT, |account| {
        let mut current = account.to_vec();
        let mut changed = false;
        for (number, value) in edits.iter().flat_map(AccountFieldEdit::fields) {
            if let Some(edited) = set_bytes(&current, number, &value)? {
                current = edited;
                changed = true;
            }
        }
        Ok(changed.then_some(current))
    })
}

/// Changes our profile's fields in the account record, for the whole account.
pub(crate) async fn write_account_fields(manager: &SignalManager, edits: &[AccountFieldEdit]) -> anyhow::Result<()> {
    write_account_with(manager, |record| edit_account_fields(record, edits)).await
}

/// Rewrites the account record with `edit`, retrying when another device
/// wrote first.
async fn write_account_with(
    manager: &SignalManager,
    edit: impl Fn(&[u8]) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<()> {
    let session = Session::new(manager).await?;
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
        let Some(record) = edit(&record)? else { return Ok(()) };
        if session.commit(manager, &manifest, Some(&old_key), IdentifierType::Account, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
}

const RECORD_STORY_LIST: u32 = 5; // StorageRecord.storyDistributionList
// StoryDistributionListRecord's fields, all but `identifier` (1).
const LIST_NAME: u32 = 2;
const LIST_MEMBERS: u32 = 3;
const LIST_DELETED_AT: u32 = 4;
const LIST_ALLOWS_REPLIES: u32 = 5;
const LIST_IS_BLOCK_LIST: u32 = 6;
const LIST_MEMBERS_BINARY: u32 = 7;

/// Sets a story list record's name, members, replies and block list (or
/// marks it deleted, at `deleted_at` unix millis, as Desktop's
/// `deleteDistributionList` does: no name, no members), keeping its
/// identifier and any fields this client doesn't know. Members go in both
/// forms, as the pins do. `Ok(None)` when nothing changes.
pub(crate) fn edit_story_list(record: &[u8], list: &crate::StoryList, deleted_at: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let edited = edit_inner(record, RECORD_STORY_LIST, |inner| {
        let fields = wire::fields(inner).context("malformed story list record")?;
        let mut out = Vec::with_capacity(inner.len() + 64);
        for field in fields.iter().filter(|f| !(LIST_NAME..=LIST_MEMBERS_BINARY).contains(&f.number)) {
            out.extend_from_slice(field.raw);
        }
        let deleted = deleted_at != 0;
        let members: Vec<ServiceId> = if deleted {
            Vec::new()
        } else {
            list.members.iter().filter_map(|m| ServiceId::parse_from_service_id_string(m)).collect()
        };
        if !deleted && !list.name.is_empty() {
            wire::put_len(&mut out, LIST_NAME, list.name.as_bytes());
        }
        for member in &members {
            wire::put_len(&mut out, LIST_MEMBERS, member.service_id_string().as_bytes());
        }
        if deleted {
            wire::put_varint_field(&mut out, LIST_DELETED_AT, deleted_at);
        }
        if list.allows_replies && !deleted {
            wire::put_varint_field(&mut out, LIST_ALLOWS_REPLIES, 1);
        }
        if list.block_list && !deleted {
            wire::put_varint_field(&mut out, LIST_IS_BLOCK_LIST, 1);
        }
        for member in &members {
            wire::put_len(&mut out, LIST_MEMBERS_BINARY, &member.service_id_binary());
        }
        let decode = |bytes: &[u8]| presage::proto::StoryDistributionListRecord::decode(bytes).ok();
        let same = decode(inner).zip(decode(&out)).is_some_and(|(old, new)| {
            (old.name.as_str(), old.deleted_at_timestamp, old.allows_replies, old.is_block_list)
                == (new.name.as_str(), new.deleted_at_timestamp, new.allows_replies, new.is_block_list)
                && story_list_members(&old) == story_list_members(&new)
        });
        Ok((!same).then_some(out))
    })?;
    Ok(edited)
}

/// A new, empty record for story list `id`.
fn new_story_list_record(id: &Uuid) -> Vec<u8> {
    let list = presage::proto::StoryDistributionListRecord { identifier: id.as_bytes().to_vec(), ..Default::default() };
    StorageRecord { record: Some(Record::StoryDistributionList(list)) }.encode_to_vec()
}

fn is_story_list(record: &StorageRecord, id: &Uuid) -> bool {
    matches!(&record.record, Some(Record::StoryDistributionList(list)) if list.identifier == id.as_bytes())
}

/// Saves a story list (Desktop's `StoryDistributionListRecord`) for the
/// whole account, adding it if Storage Service doesn't have it; with
/// `deleted_at`, marks it deleted.
pub(crate) async fn write_story_list(manager: &SignalManager, list: &crate::StoryList, deleted_at: u64) -> anyhow::Result<()> {
    let id = Uuid::parse_str(&list.id).context("not a story list id")?;
    let session = Session::new(manager).await?;
    let kind = IdentifierType::StoryDistributionList;
    for _ in 0..3 {
        let manifest = session.manifest().await?;
        let ikm = manifest_ikm(&manifest);
        let keys: Vec<Vec<u8>> =
            manifest.identifiers.iter().filter(|key| key.r#type() == kind).map(|key| key.raw.clone()).collect();
        let mut existing = None;
        for batch in keys.chunks(READ_BATCH) {
            existing = session
                .read_many(batch, ikm)
                .await?
                .into_iter()
                .find(|(_, raw)| StorageRecord::decode(raw.as_slice()).is_ok_and(|record| is_story_list(&record, &id)));
            if existing.is_some() {
                break;
            }
        }
        let (old_key, record) = match existing {
            Some((key, raw)) => (Some(key), raw),
            None => (None, new_story_list_record(&id)),
        };
        let Some(record) = edit_story_list(&record, list, deleted_at)? else { return Ok(()) };
        if session.commit(manager, &manifest, old_key.as_deref(), kind, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
}

const RECORD_CALL_LINK: u32 = 7; // StorageRecord.callLink
const LINK_ADMIN_KEY: u32 = 2;
const LINK_DELETED_AT: u32 = 3;

/// Sets a call link record's admin key, or marks it deleted at
/// `deleted_at` (unix millis) with no admin key, as Desktop's
/// `toCallLinkRecord` writes it, keeping its root key and any fields this
/// client doesn't know. `Ok(None)` when nothing changes.
pub(crate) fn edit_call_link(record: &[u8], admin_key: Option<&[u8]>, deleted_at: u64) -> anyhow::Result<Option<Vec<u8>>> {
    edit_inner(record, RECORD_CALL_LINK, |inner| {
        let fields = wire::fields(inner).context("malformed call link record")?;
        let mut out = Vec::with_capacity(inner.len() + 32);
        for field in fields.iter().filter(|f| !matches!(f.number, LINK_ADMIN_KEY | LINK_DELETED_AT)) {
            out.extend_from_slice(field.raw);
        }
        match (deleted_at, admin_key) {
            (0, Some(key)) if !key.is_empty() => wire::put_len(&mut out, LINK_ADMIN_KEY, key),
            (0, _) => {}
            (at, _) => wire::put_varint_field(&mut out, LINK_DELETED_AT, at),
        }
        let decode = |bytes: &[u8]| presage::proto::CallLinkRecord::decode(bytes).ok();
        let same = decode(inner).zip(decode(&out)).is_some_and(|(old, new)| {
            (old.admin_passkey, old.deleted_at_timestamp_ms) == (new.admin_passkey, new.deleted_at_timestamp_ms)
        });
        Ok((!same).then_some(out))
    })
}

fn new_call_link_record(root_key: &[u8]) -> Vec<u8> {
    let link = presage::proto::CallLinkRecord { root_key: root_key.to_vec(), ..Default::default() };
    StorageRecord { record: Some(Record::CallLink(link)) }.encode_to_vec()
}

fn is_call_link(record: &StorageRecord, room_id: &[u8]) -> bool {
    matches!(&record.record, Some(Record::CallLink(link))
        if signal_calls::room_id(&link.root_key).is_some_and(|room| room == room_id))
}

/// Saves a call link we're admin of for the whole account (Desktop's
/// `CallLinkRecord`), adding it if Storage Service doesn't have it; with
/// `deleted_at`, marks it deleted.
pub(crate) async fn write_call_link(
    manager: &SignalManager,
    root_key: &[u8],
    admin_key: Option<&[u8]>,
    deleted_at: u64,
) -> anyhow::Result<()> {
    let room_id = signal_calls::room_id(root_key).context("not a call link's key")?;
    let session = Session::new(manager).await?;
    let kind = IdentifierType::CallLink;
    for _ in 0..3 {
        let manifest = session.manifest().await?;
        let ikm = manifest_ikm(&manifest);
        let keys: Vec<Vec<u8>> =
            manifest.identifiers.iter().filter(|key| key.r#type() == kind).map(|key| key.raw.clone()).collect();
        let mut existing = None;
        for batch in keys.chunks(READ_BATCH) {
            existing = session
                .read_many(batch, ikm)
                .await?
                .into_iter()
                .find(|(_, raw)| StorageRecord::decode(raw.as_slice()).is_ok_and(|record| is_call_link(&record, &room_id)));
            if existing.is_some() {
                break;
            }
        }
        let (old_key, record) = match existing {
            Some((key, raw)) => (Some(key), raw),
            None => (None, new_call_link_record(root_key)),
        };
        let Some(record) = edit_call_link(&record, admin_key, deleted_at)? else { return Ok(()) };
        if session.commit(manager, &manifest, old_key.as_deref(), kind, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
}

const RECORD_CHAT_FOLDER: u32 = 8; // StorageRecord.chatFolder
/// ChatFolderRecord's fields after its id (1): name (2) to
/// deletedAtTimestampMs (11).
const FOLDER_FIELDS: std::ops::RangeInclusive<u32> = 2..=11;

/// A Storage Service recipient as one of our chats: an ACI or a group's
/// master key (Desktop's `recipientsToConversationIds`, which finds the
/// conversation; others are left out).
fn recipient_chat(recipient: &presage::proto::Recipient) -> Option<ConversationId> {
    use presage::proto::recipient::Identifier;
    match recipient.identifier.as_ref()? {
        Identifier::Contact(contact) => {
            let aci = Aci::parse_from_service_id_binary(&contact.service_id_binary)
                .or_else(|| Aci::parse_from_service_id_string(&contact.service_id))?;
            Some(ConversationId::direct(&aci.service_id_string()))
        }
        Identifier::GroupMasterKey(key) if key.len() == 32 => Some(ConversationId::group(key)),
        _ => None,
    }
}

/// A chat as a Storage Service recipient (Desktop's `toRecipient`): its
/// service id in both forms, or its group's master key.
fn chat_recipient(chat: &ConversationId) -> Option<presage::proto::Recipient> {
    use presage::proto::recipient::{Contact, Identifier};
    let identifier = match chat.as_group() {
        Some(key) => Identifier::GroupMasterKey(key),
        None => {
            let sid = ServiceId::parse_from_service_id_string(chat.as_direct()?)?;
            Identifier::Contact(Contact {
                service_id: sid.service_id_string(),
                e164: String::new(),
                service_id_binary: sid.service_id_binary(),
            })
        }
    };
    Some(presage::proto::Recipient { identifier: Some(identifier) })
}

/// A chat folder record (Desktop's `mergeChatFolderRecord`); none without
/// a 16-byte id.
pub(crate) fn chat_folder(record: &presage::proto::ChatFolderRecord) -> Option<signal_store::ChatFolder> {
    let id = Uuid::from_slice(&record.identifier).ok()?;
    Some(signal_store::ChatFolder {
        id: id.to_string(),
        folder_type: signal_store::ChatFolderType::from_wire(record.folder_type),
        name: record.name.clone(),
        position: record.position,
        show_only_unread: record.show_only_unread,
        show_muted_chats: record.show_muted_chats,
        include_all_individual_chats: record.include_all_individual_chats,
        include_all_group_chats: record.include_all_group_chats,
        included: record.included_recipients.iter().filter_map(recipient_chat).collect(),
        excluded: record.excluded_recipients.iter().filter_map(recipient_chat).collect(),
        deleted_at: record.deleted_at_timestamp_ms,
    })
}

/// Sets a chat folder record's fields to `folder`'s (Desktop's
/// `toChatFolderRecord`), keeping its id and any fields this client
/// doesn't know. `Ok(None)` when nothing changes.
pub(crate) fn edit_chat_folder(record: &[u8], folder: &signal_store::ChatFolder) -> anyhow::Result<Option<Vec<u8>>> {
    edit_inner(record, RECORD_CHAT_FOLDER, |inner| {
        let fields = wire::fields(inner).context("malformed chat folder record")?;
        let mut out = Vec::with_capacity(inner.len() + 64);
        for field in fields.iter().filter(|f| !FOLDER_FIELDS.contains(&f.number)) {
            out.extend_from_slice(field.raw);
        }
        let known = presage::proto::ChatFolderRecord {
            identifier: Vec::new(),
            name: folder.name.clone(),
            position: folder.position,
            show_only_unread: folder.show_only_unread,
            show_muted_chats: folder.show_muted_chats,
            include_all_individual_chats: folder.include_all_individual_chats,
            include_all_group_chats: folder.include_all_group_chats,
            folder_type: folder.folder_type.to_wire(),
            included_recipients: folder.included.iter().filter_map(chat_recipient).collect(),
            excluded_recipients: folder.excluded.iter().filter_map(chat_recipient).collect(),
            deleted_at_timestamp_ms: folder.deleted_at,
        };
        out.extend_from_slice(&known.encode_to_vec());
        let decode = |bytes: &[u8]| presage::proto::ChatFolderRecord::decode(bytes).ok().and_then(|r| chat_folder(&r));
        let same = decode(inner).is_some_and(|old| old == *folder);
        Ok((!same).then_some(out))
    })
}

fn new_chat_folder_record(id: &Uuid) -> Vec<u8> {
    let folder = presage::proto::ChatFolderRecord { identifier: id.as_bytes().to_vec(), ..Default::default() };
    StorageRecord { record: Some(Record::ChatFolder(folder)) }.encode_to_vec()
}

fn is_chat_folder(record: &StorageRecord, id: &Uuid) -> bool {
    matches!(&record.record, Some(Record::ChatFolder(folder)) if folder.identifier == id.as_bytes())
}

/// Saves a chat folder for the whole account (Desktop's `ChatFolderRecord`),
/// adding it if Storage Service doesn't have it.
pub(crate) async fn write_chat_folder(manager: &SignalManager, folder: &signal_store::ChatFolder) -> anyhow::Result<()> {
    let id = Uuid::parse_str(&folder.id).context("not a chat folder id")?;
    let session = Session::new(manager).await?;
    let kind = IdentifierType::ChatFolder;
    for _ in 0..3 {
        let manifest = session.manifest().await?;
        let ikm = manifest_ikm(&manifest);
        let keys: Vec<Vec<u8>> =
            manifest.identifiers.iter().filter(|key| key.r#type() == kind).map(|key| key.raw.clone()).collect();
        let mut existing = None;
        for batch in keys.chunks(READ_BATCH) {
            existing = session
                .read_many(batch, ikm)
                .await?
                .into_iter()
                .find(|(_, raw)| StorageRecord::decode(raw.as_slice()).is_ok_and(|record| is_chat_folder(&record, &id)));
            if existing.is_some() {
                break;
            }
        }
        let (old_key, record) = match existing {
            Some((key, raw)) => (Some(key), raw),
            None => (None, new_chat_folder_record(&id)),
        };
        let Some(record) = edit_chat_folder(&record, folder)? else { return Ok(()) };
        if session.commit(manager, &manifest, old_key.as_deref(), kind, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
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
    write_chat_record(manager, conversation, |record| edit_chat_record(record, edit)).await
}

/// Edits several of a record's flags at once: `Ok(None)` when none changes.
pub(crate) fn edit_chat_record_all(record: &[u8], edits: &[ChatEdit]) -> anyhow::Result<Option<Vec<u8>>> {
    let mut current: Option<Vec<u8>> = None;
    for edit in edits {
        if let Some(edited) = edit_chat_record(current.as_deref().unwrap_or(record), *edit)? {
            current = Some(edited);
        }
    }
    Ok(current)
}

/// Changes several flags on a chat for the whole account in one write
/// (accepting a message request unblocks and shares our profile at once).
pub(crate) async fn write_chat_all(manager: &SignalManager, conversation: &ConversationId, edits: &[ChatEdit]) -> anyhow::Result<()> {
    if conversation.is_signal() || edits.is_empty() {
        return Ok(());
    }
    write_chat_record(manager, conversation, |record| edit_chat_record_all(record, edits)).await
}

const CONTACT_NICKNAME: u32 = 22; // ContactRecord.nickname
const CONTACT_NOTE: u32 = 23; // ContactRecord.note
const NAME_GIVEN: u32 = 1; // ContactRecord.Name.given
const NAME_FAMILY: u32 = 2; // ContactRecord.Name.family

/// Sets a contact record's nickname and note (Desktop's `toContactRecord`
/// in `ts/services/storageRecordOps.preload.ts`: a `nickname` only when
/// either name is set, a `note` only when there is one), keeping every
/// other field, unknown ones in `nickname` too. `Ok(None)` when they're
/// already so.
pub(crate) fn edit_contact_nickname(record: &[u8], nickname: &signal_store::ContactNickname) -> anyhow::Result<Option<Vec<u8>>> {
    edit_inner(record, RECORD_CONTACT, |contact| {
        let fields = wire::fields(contact).context("malformed contact record")?;
        let current = ContactRecord::decode(contact).context("malformed contact record")?;
        let (given, family, note) = (nickname.given.trim(), nickname.family.trim(), nickname.note.trim());
        let (old_given, old_family) =
            current.nickname.as_ref().map_or(("", ""), |n| (n.given.as_str(), n.family.as_str()));
        if (old_given, old_family, current.note.as_str()) == (given, family, note) {
            return Ok(None);
        }
        // proto3 merges repeated message fields: the last one's names win,
        // and its other fields add up. Keep the unknown ones of each.
        let mut name = Vec::new();
        for field in fields.iter().filter(|f| f.number == CONTACT_NICKNAME) {
            let inner = field.payload.and_then(wire::fields).context("malformed nickname")?;
            for kept in inner.iter().filter(|f| !matches!(f.number, NAME_GIVEN | NAME_FAMILY)) {
                name.extend_from_slice(kept.raw);
            }
        }
        if !given.is_empty() {
            wire::put_len(&mut name, NAME_GIVEN, given.as_bytes());
        }
        if !family.is_empty() {
            wire::put_len(&mut name, NAME_FAMILY, family.as_bytes());
        }
        let mut out = Vec::with_capacity(contact.len() + name.len() + note.len() + 8);
        for field in fields.iter().filter(|f| !matches!(f.number, CONTACT_NICKNAME | CONTACT_NOTE)) {
            out.extend_from_slice(field.raw);
        }
        if !given.is_empty() || !family.is_empty() {
            wire::put_len(&mut out, CONTACT_NICKNAME, &name);
        }
        if !note.is_empty() {
            wire::put_len(&mut out, CONTACT_NOTE, note.as_bytes());
        }
        Ok(Some(out))
    })
}

/// Sets the nickname and note we give someone, for the whole account.
pub(crate) async fn write_nickname(
    manager: &SignalManager,
    conversation: &ConversationId,
    nickname: &signal_store::ContactNickname,
) -> anyhow::Result<()> {
    ensure!(conversation.as_group().is_none() && !conversation.is_signal(), "only a person can have a nickname");
    write_chat_record(manager, conversation, |record| edit_contact_nickname(record, nickname)).await
}

/// Edits a chat's contact or group record with `edit`, adding the record
/// if Storage Service has none.
async fn write_chat_record(
    manager: &SignalManager,
    conversation: &ConversationId,
    edit: impl Fn(&[u8]) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<()> {
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
        let Some(record) = edit(&record)? else { return Ok(()) };
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
    Some(crate::StoryList {
        id: id.to_string(),
        name: record.name.clone(),
        members: story_list_members(record),
        block_list: record.is_block_list,
        allows_replies: record.allows_replies,
    })
}

/// A story list's members as service id strings, from the binary form when
/// it's there (as Desktop reads them).
fn story_list_members(record: &presage::proto::StoryDistributionListRecord) -> Vec<String> {
    let members = if record.recipient_service_ids_binary.is_empty() {
        record.recipient_service_ids.iter().filter_map(|s| ServiceId::parse_from_service_id_string(s)).collect::<Vec<_>>()
    } else {
        record.recipient_service_ids_binary.iter().filter_map(|b| ServiceId::parse_from_service_id_binary(b)).collect()
    };
    members.iter().map(ServiceId::service_id_string).collect()
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

/// What a contact record says about someone. Their name follows Desktop's
/// `getTitleNoDefault` (`ts/util/getTitle.preload.ts`): the nickname we gave
/// them, then their address-book name (its nickname, else given and family
/// names), then their profile name; names combine as `combineNames` does.
pub(crate) fn from_record(record: &ContactRecord) -> Option<StorageContact> {
    use signal_store::combine_names;
    let aci = record_aci(record)?;
    let nickname = signal_store::ContactNickname {
        given: record.nickname.as_ref().map(|n| n.given.trim().to_owned()).unwrap_or_default(),
        family: record.nickname.as_ref().map(|n| n.family.trim().to_owned()).unwrap_or_default(),
        note: record.note.trim().to_owned(),
        plain_name: Some(record.system_nickname.trim().to_owned())
            .filter(|n| !n.is_empty())
            .or_else(|| combine_names(&record.system_given_name, &record.system_family_name))
            .or_else(|| combine_names(&record.given_name, &record.family_name)),
    };
    Some(StorageContact {
        aci,
        name: nickname.nickname().or_else(|| nickname.plain_name.clone()),
        profile_key: record.profile_key.as_slice().try_into().ok(),
        e164: Some(record.e164.clone()).filter(|e| !e.is_empty()),
        hidden: record.hidden || record.blocked,
        system_name: [&record.system_given_name, &record.system_family_name, &record.system_nickname]
            .iter()
            .any(|n| !n.trim().is_empty()),
        nickname,
        username: Some(record.username.trim().to_owned()).filter(|u| !u.is_empty()),
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
        record.note = " Owes me lunch ".into();
        let c = from_record(&record).unwrap();
        assert_eq!(c.name.as_deref(), Some("J"));
        assert_eq!(
            c.nickname,
            signal_store::ContactNickname {
                given: "J".into(),
                family: String::new(),
                note: "Owes me lunch".into(),
                plain_name: Some("Jon".into())
            }
        );
        // A family name alone is a nickname too.
        record.nickname = Some(Name { given: String::new(), family: "Doe".into() });
        assert_eq!(from_record(&record).unwrap().name.as_deref(), Some("Doe"));
    }

    fn contact_record(contact: &ContactRecord, extra_contact: &[u8]) -> Vec<u8> {
        let mut inner = contact.encode_to_vec();
        inner.extend_from_slice(extra_contact);
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_CONTACT, &inner);
        record
    }

    fn decode_contact(record: &[u8]) -> ContactRecord {
        match StorageRecord::decode(record).unwrap().record {
            Some(Record::Contact(contact)) => contact,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nickname_edits_change_only_the_nickname_and_note() {
        let nick = |given: &str, family: &str, note: &str| signal_store::ContactNickname {
            given: given.into(),
            family: family.into(),
            note: note.into(),
            plain_name: None,
        };
        let contact = ContactRecord { aci: ACI.into(), given_name: "Maurice".into(), archived: true, ..Default::default() };
        // A field this client doesn't know in the contact record, and one in a nickname.
        let mut unknown = Vec::new();
        wire::put_len(&mut unknown, 99, b"future contact field");
        let mut name = Vec::new();
        wire::put_len(&mut name, NAME_GIVEN, b"Old");
        wire::put_len(&mut name, 7, b"future name field");
        wire::put_len(&mut unknown, CONTACT_NICKNAME, &name);
        let record = contact_record(&contact, &unknown);
        let contains = |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);

        let edited = edit_contact_nickname(&record, &nick(" Mo ", "", "Met at the gym")).unwrap().unwrap();
        let decoded = decode_contact(&edited);
        assert_eq!(decoded.nickname.as_ref().map(|n| (n.given.as_str(), n.family.as_str())), Some(("Mo", "")));
        assert_eq!(decoded.note, "Met at the gym");
        assert!(decoded.archived && decoded.given_name == "Maurice");
        assert!(contains(&edited, b"future contact field") && contains(&edited, b"future name field"));
        assert!(!contains(&edited, b"Old"));
        assert_eq!(edit_contact_nickname(&edited, &nick("Mo", "", "Met at the gym")).unwrap(), None, "already so");

        // Deleted, as Desktop's "Delete": no nickname and no note.
        let cleared = edit_contact_nickname(&edited, &nick("", "", "")).unwrap().unwrap();
        let decoded = decode_contact(&cleared);
        assert!(decoded.nickname.is_none() && decoded.note.is_empty());
        assert!(contains(&cleared, b"future contact field"));
        assert_eq!(from_record(&decoded).unwrap().name.as_deref(), Some("Maurice"));

        // On a record made for someone storage didn't know yet.
        let (_, fresh) = new_chat_record(&ConversationId::direct(ACI)).unwrap();
        let named = decode_contact(&edit_contact_nickname(&fresh, &nick("", "Diaz", "")).unwrap().unwrap());
        assert_eq!(from_record(&named).unwrap().nickname.family, "Diaz");
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
        let limit = crate::remote_config().pinned_chat_limit();
        let pins = (0..limit as u8).map(|i| pin_for(&ConversationId::group(&[i; 32])).unwrap()).collect();
        let record = account_record(&AccountRecord { pinned_conversations: pins, ..Default::default() });
        let error = edit_pins(&record, &PinEdit::Pin(ConversationId::direct(ACI))).unwrap_err();
        assert!(error.to_string().contains(&format!("up to {limit}")), "{error}");
        // Unpinning still works at the limit.
        assert!(edit_pins(&record, &PinEdit::Unpin(ConversationId::group(&[0; 32]))).unwrap().is_some());
    }

    #[test]
    fn profile_field_edits_keep_unknown_fields() {
        use crate::usernames::UsernameLink;
        let contains = |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
        let account = AccountRecord { given_name: "Old".into(), read_receipts: true, ..Default::default() };
        let mut account_bytes = account.encode_to_vec();
        wire::put_len(&mut account_bytes, 999, b"future account field");
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_ACCOUNT, &account_bytes);
        let link = UsernameLink { entropy: vec![1; 32], server_id: vec![2; 16], color: 5 };
        let edits = [
            AccountFieldEdit::Name { given: "Sam".into(), family: "Rivera".into() },
            AccountFieldEdit::AvatarPath(Some("profiles/abc".into())),
            AccountFieldEdit::Username(Some("trail_mix.42".into())),
            AccountFieldEdit::UsernameLink(Some(link.clone())),
        ];
        let edited = edit_account_fields(&record, &edits).unwrap().unwrap();
        let account = decode_account(&edited);
        assert_eq!((account.given_name.as_str(), account.family_name.as_str()), ("Sam", "Rivera"));
        assert_eq!((account.avatar_url_path.as_str(), account.username.as_str()), ("profiles/abc", "trail_mix.42"));
        assert!(account.read_receipts);
        assert!(contains(&edited, b"future account field"));
        let own = OwnProfileRecord::from_record(&account);
        assert_eq!(own.username_link, Some(link));
        assert_eq!(own.avatar_url_path.as_deref(), Some("profiles/abc"));
        assert!(own.sharing_number, "unset shares, as Desktop merges it");
        // Again changes nothing; clearing leaves the fields out.
        assert_eq!(edit_account_fields(&edited, &edits).unwrap(), None);
        let cleared = edit_account_fields(
            &edited,
            &[AccountFieldEdit::Username(None), AccountFieldEdit::UsernameLink(None), AccountFieldEdit::AvatarPath(None)],
        )
        .unwrap()
        .unwrap();
        let account = decode_account(&cleared);
        assert_eq!((account.username.as_str(), account.username_link, account.avatar_url_path.as_str()), ("", None, ""));
        assert!(contains(&cleared, b"future account field"));
    }

    #[test]
    fn contact_records_carry_usernames() {
        let record = ContactRecord {
            aci: ACI.into(),
            system_given_name: "Maya".into(),
            system_family_name: "Chen".into(),
            username: "maya.42".into(),
            ..Default::default()
        };
        let contact = from_record(&record).unwrap();
        assert_eq!((contact.system_name, contact.username.as_deref()), (true, Some("maya.42")));
        let record = ContactRecord { aci: ACI.into(), given_name: "Maya".into(), ..Default::default() };
        let contact = from_record(&record).unwrap();
        assert_eq!((contact.system_name, contact.username), (false, None));
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
    fn profile_sharing_and_story_send_mode_write_their_fields() {
        let contact = StorageRecord { record: Some(Record::Contact(ContactRecord { aci: ACI.into(), ..Default::default() })) };
        let shared = edit_chat_record(&contact.encode_to_vec(), ChatEdit::ProfileSharing(true)).unwrap().unwrap();
        let Some(Record::Contact(c)) = StorageRecord::decode(shared.as_slice()).unwrap().record else { panic!() };
        assert!(c.whitelisted);
        // A contact has no story send mode.
        assert!(edit_chat_record(&contact.encode_to_vec(), ChatEdit::StorySendMode(signal_store::StorySendMode::Never)).is_err());

        let group = StorageRecord { record: Some(Record::GroupV2(GroupV2Record { master_key: vec![1; 32], ..Default::default() })) };
        let removed =
            edit_chat_record(&group.encode_to_vec(), ChatEdit::StorySendMode(signal_store::StorySendMode::Never)).unwrap().unwrap();
        let Some(Record::GroupV2(g)) = StorageRecord::decode(removed.as_slice()).unwrap().record else { panic!() };
        assert_eq!(g.story_send_mode(), presage::proto::group_v2_record::StorySendMode::Disabled);
        let shared = edit_chat_record(&removed, ChatEdit::ProfileSharing(true)).unwrap().unwrap();
        let Some(Record::GroupV2(g)) = StorageRecord::decode(shared.as_slice()).unwrap().record else { panic!() };
        assert!(g.whitelisted && g.story_send_mode() == presage::proto::group_v2_record::StorySendMode::Disabled);
    }

    #[test]
    fn accepting_a_blocked_request_unblocks_and_shares_in_one_edit() {
        let record = StorageRecord {
            record: Some(Record::Contact(ContactRecord { aci: ACI.into(), blocked: true, archived: true, ..Default::default() })),
        };
        let edits = [ChatEdit::Blocked(false), ChatEdit::ProfileSharing(true)];
        let edited = edit_chat_record_all(&record.encode_to_vec(), &edits).unwrap().unwrap();
        let Some(Record::Contact(c)) = StorageRecord::decode(edited.as_slice()).unwrap().record else { panic!() };
        assert!(!c.blocked && c.whitelisted && c.archived, "other fields kept");
        // Nothing to change: no write.
        assert_eq!(edit_chat_record_all(&edited, &edits).unwrap(), None);
        // One of two changing is still a change.
        let edits = [ChatEdit::Blocked(false), ChatEdit::ProfileSharing(false)];
        let Some(Record::Contact(c)) =
            StorageRecord::decode(edit_chat_record_all(&edited, &edits).unwrap().unwrap().as_slice()).unwrap().record
        else {
            panic!()
        };
        assert!(!c.whitelisted && !c.blocked);
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
    fn account_story_settings_keep_every_other_field() {
        let account = AccountRecord {
            given_name: "Ken".into(),
            pinned_conversations: vec![pin_for(&ConversationId::group(&[0xab; 32])).unwrap()],
            ..Default::default()
        };
        let mut account_bytes = account.encode_to_vec();
        wire::put_len(&mut account_bytes, 999, b"future account field");
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_ACCOUNT, &account_bytes);

        let edits = [AccountEdit::StoriesDisabled(true), AccountEdit::StoryViewReceipts(false)];
        let edited = edit_account(&record, &edits).unwrap().unwrap();
        let decoded = decode_account(&edited);
        assert!(decoded.stories_disabled);
        assert_eq!(decoded.story_view_receipts_enabled(), presage::proto::OptionalBool::Disabled);
        assert_eq!((decoded.given_name.as_str(), decoded.pinned_conversations.len()), ("Ken", 1));
        assert!(edited.windows(20).any(|w| w == b"future account field"));
        assert_eq!(edit_account(&edited, &edits).unwrap(), None, "already so");

        let edited = edit_account(&edited, &[AccountEdit::StoryViewReceipts(true), AccountEdit::HasSetMyStoriesPrivacy(true)])
            .unwrap()
            .unwrap();
        let decoded = decode_account(&edited);
        assert_eq!(decoded.story_view_receipts_enabled(), presage::proto::OptionalBool::Enabled);
        assert!(decoded.has_set_my_stories_privacy && decoded.stories_disabled);
        assert!(edit_account(&edited, &[AccountEdit::StoriesDisabled(false)]).unwrap().is_some());

        // Automatic key verification, Desktop's field 46.
        let edited = edit_account(&edited, &[AccountEdit::AutomaticKeyVerificationDisabled(true)]).unwrap().unwrap();
        let decoded = decode_account(&edited);
        assert!(decoded.automatic_key_verification_disabled && decoded.stories_disabled);
        assert!(edited.windows(20).any(|w| w == b"future account field"));
    }

    #[test]
    fn account_identifiers_for_key_transparency() {
        let account = AccountRecord {
            username: "maya.01".into(),
            unlisted_phone_number: true,
            automatic_key_verification_disabled: true,
            profile_key: vec![1; 32],
            ..Default::default()
        };
        let ids = AccountIdentifiers::from_record(&account);
        assert_eq!(ids.username.as_deref(), Some("maya.01"));
        assert!(ids.unlisted_phone_number && ids.key_transparency_disabled && ids.has_profile_key);
        let empty = AccountIdentifiers::from_record(&AccountRecord::default());
        assert_eq!(empty, AccountIdentifiers::default());
    }

    #[test]
    fn privacy_settings_read_and_write_the_account_record() {
        use crate::AccountSetting;
        let account = AccountRecord {
            given_name: "Ken".into(),
            typing_indicators: true,
            keep_muted_chats_archived: true,
            universal_expire_timer: 3600,
            notify_for_calls_if_muted: presage::proto::OptionalBool::Enabled.into(),
            notify_for_replies_if_muted: presage::proto::OptionalBool::Disabled.into(),
            ..Default::default()
        };
        // Desktop takes every one: a flag left out is off, an unset
        // OptionalBool the default, an unset sharing mode everybody.
        let settings = account_settings(&account);
        let on = |s: AccountSetting| settings.flags[&s];
        assert!(!on(AccountSetting::ReadReceipts) && on(AccountSetting::TypingIndicators));
        assert!(!on(AccountSetting::LinkPreviews) && !on(AccountSetting::SealedSenderIndicators));
        assert!(on(AccountSetting::KeepMutedChatsArchived) && !on(AccountSetting::PreferContactAvatars));
        assert!(on(AccountSetting::ReactionNotifications), "unset is on");
        assert!(on(AccountSetting::NotifyCallsIfMuted) && !on(AccountSetting::NotifyRepliesIfMuted));
        assert!(on(AccountSetting::NotifyMentionsIfMuted) && !on(AccountSetting::ShowUnreadReminders));
        assert_eq!(settings.flags.len(), AccountSetting::ALL.len());
        assert_eq!((settings.universal_expire_timer, settings.phone_number_sharing), (Some(3600), Some(true)));
        let nobody = AccountRecord { phone_number_sharing_mode: 2, ..Default::default() };
        assert_eq!(account_settings(&nobody).phone_number_sharing, Some(false));
        let mut account_bytes = account.encode_to_vec();
        wire::put_len(&mut account_bytes, 999, b"future account field");
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_ACCOUNT, &account_bytes);

        let edits = [
            AccountEdit::Setting(AccountSetting::ReadReceipts, true),
            AccountEdit::Setting(AccountSetting::TypingIndicators, false),
            AccountEdit::Setting(AccountSetting::LinkPreviews, true),
            AccountEdit::Setting(AccountSetting::SealedSenderIndicators, true),
        ];
        let edited = edit_account(&record, &edits).unwrap().unwrap();
        let decoded = decode_account(&edited);
        assert!(decoded.read_receipts && !decoded.typing_indicators && decoded.link_previews && decoded.sealed_sender_indicators);
        assert_eq!(decoded.given_name, "Ken");
        assert!(edited.windows(20).any(|w| w == b"future account field"));
        assert_eq!(edit_account(&edited, &edits).unwrap(), None, "already so");

        // The rest, Desktop's field numbers and OptionalBool values.
        use presage::proto::OptionalBool;
        let edits = [
            AccountEdit::Setting(AccountSetting::PreferContactAvatars, true),
            AccountEdit::Setting(AccountSetting::KeepMutedChatsArchived, false),
            AccountEdit::Setting(AccountSetting::ReactionNotifications, false),
            AccountEdit::Setting(AccountSetting::NotifyCallsIfMuted, true),
            AccountEdit::Setting(AccountSetting::NotifyMentionsIfMuted, false),
            AccountEdit::Setting(AccountSetting::NotifyRepliesIfMuted, true),
            AccountEdit::Setting(AccountSetting::ShowUnreadReminders, true),
            AccountEdit::UniversalExpireTimer(86_400),
            AccountEdit::PhoneNumberSharing(false),
            AccountEdit::UnlistedPhoneNumber(true),
        ];
        let edited = edit_account(&edited, &edits).unwrap().unwrap();
        let decoded = decode_account(&edited);
        assert!(decoded.prefer_contact_avatars && !decoded.keep_muted_chats_archived);
        assert_eq!(decoded.reaction_notifications(), OptionalBool::Disabled);
        assert_eq!(decoded.notify_for_calls_if_muted(), OptionalBool::Enabled);
        assert_eq!(decoded.notify_for_mentions_if_muted(), OptionalBool::Disabled);
        assert_eq!(decoded.notify_for_replies_if_muted(), OptionalBool::Enabled);
        assert_eq!(decoded.show_unread_reminders(), OptionalBool::Enabled);
        assert_eq!((decoded.universal_expire_timer, decoded.phone_number_sharing_mode), (86_400, 2));
        assert!(decoded.unlisted_phone_number && decoded.read_receipts);
        assert!(edited.windows(20).any(|w| w == b"future account field"));
        let everybody = edit_account(&edited, &[AccountEdit::PhoneNumberSharing(true), AccountEdit::UniversalExpireTimer(0)]);
        let decoded = decode_account(&everybody.unwrap().unwrap());
        assert_eq!((decoded.universal_expire_timer, decoded.phone_number_sharing_mode), (0, 1));
    }

    #[test]
    fn call_link_records_read_and_write_as_desktops() {
        use presage::proto::CallLinkRecord;
        let decode = |record: &[u8]| match StorageRecord::decode(record).unwrap().record {
            Some(Record::CallLink(link)) => link,
            other => panic!("{other:?}"),
        };
        // A V0 root key (16 bytes, no repeated pairs).
        let root_key: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let fresh = new_call_link_record(&root_key);
        let room = signal_calls::room_id(&root_key).unwrap();
        assert!(is_call_link(&StorageRecord::decode(fresh.as_slice()).unwrap(), &room));
        assert!(!is_call_link(&StorageRecord::decode(fresh.as_slice()).unwrap(), &[0; 32]));

        let saved = edit_call_link(&fresh, Some(&[7; 16]), 0).unwrap().unwrap();
        let link = decode(&saved);
        assert_eq!((link.root_key.as_slice(), link.admin_passkey.as_slice(), link.deleted_at_timestamp_ms), (&root_key[..], &[7u8; 16][..], 0));
        assert_eq!(edit_call_link(&saved, Some(&[7; 16]), 0).unwrap(), None, "already so");
        assert_eq!(call_link(&link), Some(StorageCallLink { root_key: root_key.clone(), admin_key: Some(vec![7; 16]), deleted_at: None }));

        // Unknown fields stay; deleting drops the admin key (Desktop's `toCallLinkRecord`).
        let mut inner = CallLinkRecord { root_key: root_key.clone(), admin_passkey: vec![7; 16], ..Default::default() }.encode_to_vec();
        wire::put_len(&mut inner, 99, b"future link field");
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_CALL_LINK, &inner);
        let deleted = edit_call_link(&record, None, 1234).unwrap().unwrap();
        assert!(deleted.windows(17).any(|w| w == b"future link field"));
        let link = decode(&deleted);
        assert!(link.admin_passkey.is_empty());
        assert_eq!(call_link(&link).unwrap().deleted_at, Some(1234));
        assert_eq!(call_link(&CallLinkRecord::default()), None, "no root key");
    }

    #[test]
    fn story_list_edits_write_desktops_fields_and_keep_unknown_ones() {
        use presage::proto::StoryDistributionListRecord;
        let decode = |record: &[u8]| match StorageRecord::decode(record).unwrap().record {
            Some(Record::StoryDistributionList(list)) => list,
            other => panic!("{other:?}"),
        };
        let id = Uuid::from_bytes([3; 16]);
        let mut inner = StoryDistributionListRecord {
            identifier: id.as_bytes().to_vec(),
            name: "Hikers".into(),
            recipient_service_ids: vec!["stale".into()],
            allows_replies: true,
            ..Default::default()
        }
        .encode_to_vec();
        wire::put_len(&mut inner, 99, b"future list field");
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_STORY_LIST, &inner);

        let other = "6b1a2b3c-0000-4000-8000-000000000002";
        let list = crate::StoryList {
            id: id.to_string(),
            name: "Close friends".into(),
            members: vec![ACI.into(), other.into(), "not a service id".into()],
            block_list: false,
            allows_replies: false,
        };
        let edited = edit_story_list(&record, &list, 0).unwrap().unwrap();
        let decoded = decode(&edited);
        assert_eq!(decoded.identifier, id.as_bytes());
        assert_eq!(decoded.name, "Close friends");
        assert_eq!(decoded.recipient_service_ids, [ACI, other]);
        assert_eq!(story_list_members(&decoded), [ACI, other], "the binary forms too");
        assert!(!decoded.allows_replies && !decoded.is_block_list);
        assert!(edited.windows(17).any(|w| w == b"future list field"));
        assert_eq!(story_list(&decoded).unwrap(), crate::StoryList { members: vec![ACI.into(), other.into()], ..list.clone() });
        assert_eq!(edit_story_list(&edited, &list, 0).unwrap(), None, "already so");

        // Deleted as Desktop deletes one: no name, no members.
        let deleted = decode(&edit_story_list(&edited, &list, 1234).unwrap().unwrap());
        assert_eq!(deleted.deleted_at_timestamp, 1234);
        assert!(deleted.name.is_empty() && deleted.recipient_service_ids.is_empty() && deleted.recipient_service_ids_binary.is_empty());
        assert_eq!(story_list(&deleted), None);

        // A new one, such as My Story set to "All except…".
        let my_story = crate::StoryList {
            id: crate::MY_STORY_ID.into(),
            name: String::new(),
            members: vec![ACI.into()],
            block_list: true,
            allows_replies: true,
        };
        let fresh = new_story_list_record(&Uuid::nil());
        assert!(is_story_list(&StorageRecord::decode(fresh.as_slice()).unwrap(), &Uuid::nil()));
        let saved = decode(&edit_story_list(&fresh, &my_story, 0).unwrap().unwrap());
        assert_eq!(story_list(&saved).unwrap(), my_story);
    }

    #[test]
    fn chat_folder_records_read_and_write_as_desktops() {
        let id = Uuid::parse_str("8f2c6d4e-1b3a-4c5d-9e7f-0a1b2c3d4e5f").unwrap();
        let aci = "22222222-2222-4222-8222-222222222222";
        let folder = signal_store::ChatFolder {
            id: id.to_string(),
            folder_type: signal_store::ChatFolderType::Custom,
            name: "Hikers".into(),
            position: 2,
            show_only_unread: true,
            show_muted_chats: false,
            include_all_individual_chats: false,
            include_all_group_chats: true,
            included: vec![ConversationId::direct(aci)],
            excluded: vec![ConversationId::group(&[9; 32])],
            deleted_at: 0,
        };
        // A record with a field this client doesn't know (99).
        let mut inner = presage::proto::ChatFolderRecord { identifier: id.as_bytes().to_vec(), ..Default::default() }.encode_to_vec();
        wire::put_varint_field(&mut inner, 99, 5);
        let mut record = Vec::new();
        wire::put_len(&mut record, RECORD_CHAT_FOLDER, &inner);
        let edited = edit_chat_folder(&record, &folder).unwrap().expect("changed");
        assert!(edit_chat_folder(&edited, &folder).unwrap().is_none(), "already so");
        let Some(Record::ChatFolder(written)) = StorageRecord::decode(edited.as_slice()).unwrap().record else { panic!() };
        assert_eq!(chat_folder(&written), Some(folder));
        // Desktop's recipients: the service id in both forms, or the key.
        let contact = written.included_recipients[0].identifier.clone();
        assert!(matches!(contact, Some(presage::proto::recipient::Identifier::Contact(c)) if c.service_id == aci && c.service_id_binary.len() == 16));
        let inner = wire::fields(&edited).unwrap()[0].payload.unwrap().to_vec();
        assert!(wire::fields(&inner).unwrap().iter().any(|f| f.number == 99 && f.varint == Some(5)), "unknown field kept");
        // Without a 16-byte id it isn't a folder.
        assert_eq!(chat_folder(&presage::proto::ChatFolderRecord::default()), None);
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
