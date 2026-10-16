//! The backend actor: owns the presage manager and applies incoming content to
//! the app database. Runs on a `LocalSet` because presage's futures are `!Send`.

mod call_log;
mod call_links;
mod calls;
mod early;
mod group_admin;
mod group_calls;
mod polls;
mod stories;
mod sender_keys;
mod key_transparency;
mod message_requests;
mod release_notes;
mod remote_config;
mod view_once;
mod profile;
mod settings;

pub(crate) use view_once::VIEW_ONCE_DIR;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    future, pin_mut,
};
use presage::{
    Manager,
    libsignal_service::{
        configuration::SignalServers,
        sender::AttachmentSpec,
        content::ContentBody,
        prelude::{GroupMasterKey, GroupSecretParams, ProfileKey, ServiceError},
        protocol::{Aci, DeviceId, IdentityKey, IdentityKeyStore as _, ProtocolAddress, ServiceId},
    },
    manager::Registered,
    model::messages::Received,
    model::identity::OnNewIdentity,
    proto::{
        AttachmentPointer, DataMessage, attachment_pointer, GroupContextV2, ReceiptMessage, SyncMessage, TypingMessage,
        data_message, receipt_message, sync_message, typing_message,
    },
    store::{ContentsStore, StateStore, Thread},
};
use presage_store_sqlite::{SqliteConnectOptions, SqliteStore};
use signal_store::{
    AppDb, AttachmentMeta, ConversationId, ConversationKind, LinkPreview, ConversationUpdate, DeliveryStatus, Direction,
    Message, NewMessage, Quote, Reaction, RemoteAttachment,
};
use tracing::{debug, info, warn};

use crate::{
    AccountSetting, Command, Config, ConnectionState, Event,
    VOICE_CONTENT_TYPE, mapping::{self, Action, MapContext},
    media,
    remote_config::WireServiceId,
    storage,
};
use anyhow::Context as _;

pub(crate) use call_links::clamp_name as clamp_link_name;

pub(crate) type SignalManager = Manager<SqliteStore, Registered>;

/// How checking someone's profile photo went, for the batch's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhotoOutcome {
    Changed,
    Unchanged,
    NoPhoto,
    NoKey,
    ProfileFailed,
    DownloadFailed,
}

impl PhotoOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Changed => "updated",
            Self::Unchanged => "unchanged",
            Self::NoPhoto => "no photo",
            Self::NoKey => "no profile key",
            Self::ProfileFailed => "profile fetch failed",
            Self::DownloadFailed => "download failed",
        }
    }
}

/// Images up to this size are downloaded as soon as they arrive.
/// When this device was linked (unix millis).
const SETTING_DEVICE_CREATED: &str = "device.created_at";
/// "1" while the history transfer after linking hasn't finished.
const SETTING_HISTORY_PENDING: &str = "history.pending";
/// `avatar.checked.<service id>`: when we last fetched their profile for
/// its photo (unix millis).
const SETTING_AVATAR_CHECKED: &str = "avatar.checked.";
const AVATAR_CHECK_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
/// `group.fetch_failed.<conversation id>`: when the group server last
/// refused us that group (unix millis).
const SETTING_GROUP_FETCH_FAILED: &str = "group.fetch_failed.";
const AVATAR_CHECKS_PER_SYNC: usize = 100;
const AVATAR_BATCH_DELAY: Duration = Duration::from_secs(5 * 60);
/// `verified.<ACI>`: the identity key (hex) we marked verified for them;
/// empty when cleared. Verification lapses when their key changes.
pub(crate) const SETTING_VERIFIED: &str = "verified.";
/// `identity.<ACI>`: their identity key (hex) as last seen, to notice a
/// change.
pub(crate) const SETTING_IDENTITY: &str = "identity.";

/// Whether an attachment is fetched whole as soon as it arrives: as the
/// media auto-download choices say (Data usage), under the limit.
pub(crate) fn downloads_whole(attachment: &AttachmentMeta) -> bool {
    crate::auto_download::current().downloads(attachment)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// An attachment to upload from its local copy.
struct Upload {
    path: std::path::PathBuf,
    file_name: Option<String>,
    /// A file to send unchanged, with this MIME type; `None` for an image,
    /// which is optimized first.
    file_type: Option<String>,
    /// Sent with the voice message flag.
    voice: bool,
}

/// Outbound work, processed one at a time so sessions are never ratcheted concurrently.
enum Outbound {
    Send {
        conversation: ConversationId,
        body: ContentBody,
        timestamp: u64,
        /// Our message row whose status tracks this send.
        message: Option<i64>,
    },
    /// A message whose images still need optimizing and uploading; their
    /// pointers are added to `body` before it is sent.
    SendImages {
        conversation: ConversationId,
        body: DataMessage,
        timestamp: u64,
        message: i64,
        /// Local copies (in `attachments/`), in attachment order.
        images: Vec<Upload>,
        /// The link preview's image (already optimized), for `body.preview`.
        preview_image: Option<std::path::PathBuf>,
    },
    /// A GIF: its MP4 is uploaded with Signal's GIF flag.
    SendGif {
        conversation: ConversationId,
        body: DataMessage,
        timestamp: u64,
        message: i64,
        path: std::path::PathBuf,
        size: (u32, u32),
    },
    /// A sticker message; its image is uploaded for `body.sticker.data`.
    SendSticker {
        conversation: ConversationId,
        body: DataMessage,
        timestamp: u64,
        message: i64,
        path: std::path::PathBuf,
    },
    /// Our vote numbered `vote_count` in poll `message`, to whoever it
    /// hasn't reached yet; `attempt` counts from 1 (Desktop's job queue
    /// retries, `polls.rs`).
    PollVote { message: i64, vote_count: u32, attempt: u32 },
    /// Sync message to our other devices.
    Sync { body: SyncMessage },
    /// A group update to someone outside the group's members: an invitee,
    /// or someone just removed (Desktop's `includePendingMembers` and
    /// `extraConversationsForSend`). Sent as it is, not as a 1:1 message.
    GroupUpdateTo { recipient: String, body: ContentBody, timestamp: u64 },
    /// A story (or group story reply) to one recipient, part of `job`.
    Story { recipient: String, body: ContentBody, timestamp: u64, job: u64 },
    /// A story to many at once with our sender key for `target` (a group's
    /// conversation id, or `list:<id>`; `group` is the group's master key),
    /// part of `job`. Whoever that doesn't reach gets it one by one.
    StoryBatch { target: String, group: Option<Vec<u8>>, recipients: Vec<String>, body: ContentBody, timestamp: u64, job: u64 },
}

/// Why a long text is being downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LongTextTry {
    /// It came, or its download was cut off.
    First,
    /// "Download Full Message" was clicked.
    Asked,
    /// A retry after a failure, scheduled for this time.
    Scheduled(i64),
}

/// How a send went, recipient by recipient (Desktop's
/// `sendStateByConversationId`, and the errors `handleMultipleSendErrors`
/// looks at).
#[derive(Debug, Default)]
struct Delivery {
    /// Service ids it reached.
    reached: Vec<String>,
    /// No longer registered: it never will reach them.
    unregistered: Vec<String>,
    /// Missed this time, with why.
    failed: Vec<(String, String)>,
    /// It couldn't go at all (the Signal chat, a failed upload, no group):
    /// why.
    error: Option<String>,
    /// The server said to wait this long first (rate limited).
    retry_after: Option<Duration>,
    /// The server refused it (HTTP 508 or 400): Desktop stops retrying.
    rejected: bool,
}

/// Why a send to someone failed.
struct SendFailure {
    message: String,
    unregistered: bool,
    retry_after: Option<Duration>,
    rejected: bool,
}

impl SendFailure {
    fn service(error: &ServiceError, message: String) -> Self {
        let rejected = matches!(error, ServiceError::ServerRejected)
            || matches!(error, ServiceError::UnhandledResponseCode { status, .. } if status.as_u16() == 400);
        let retry_after = match error {
            ServiceError::RateLimitExceeded { retry_after } => {
                Some(retry_after.and_then(|d| d.to_std().ok()).unwrap_or(Duration::ZERO))
            }
            _ => None,
        };
        Self { message, unregistered: false, retry_after, rejected }
    }
}

impl From<presage::libsignal_service::prelude::MessageSenderError> for SendFailure {
    fn from(error: presage::libsignal_service::prelude::MessageSenderError) -> Self {
        use presage::libsignal_service::prelude::MessageSenderError as E;
        let message = error.to_string();
        match &error {
            E::NotFound { .. } => Self { message, unregistered: true, retry_after: None, rejected: false },
            E::ServiceError(service) => Self::service(service, message),
            _ => Self { message, unregistered: false, retry_after: None, rejected: false },
        }
    }
}

impl<S: std::error::Error> From<presage::Error<S>> for SendFailure {
    fn from(error: presage::Error<S>) -> Self {
        match error {
            presage::Error::MessageSenderError(error) => Self::from(*error),
            presage::Error::ServiceError(service) => {
                let message = service.to_string();
                Self::service(&service, message)
            }
            other => Self { message: other.to_string(), unregistered: false, retry_after: None, rejected: false },
        }
    }
}

impl Delivery {
    fn note(&mut self, failure: &SendFailure) {
        self.rejected |= failure.rejected;
        if let Some(wait) = failure.retry_after {
            self.retry_after = Some(self.retry_after.map_or(wait, |w| w.max(wait)));
        }
    }

    fn record(&mut self, recipient: String, result: Result<(), SendFailure>) {
        match result {
            Ok(()) => self.reached.push(recipient),
            Err(failure) if failure.unregistered => self.unregistered.push(recipient),
            Err(failure) => {
                self.note(&failure);
                self.failed.push((recipient, failure.message));
            }
        }
    }
}

impl Outbound {
    /// The one-to-one chat a message of ours goes to (not a receipt,
    /// typing indicator or sync), with which Desktop shares our profile
    /// from then on (`enqueueMessageForSend`'s "mandatoryProfileSharing").
    fn message_to(&self) -> Option<&ConversationId> {
        self.message_to_any().filter(|c| c.as_direct().is_some())
    }

    /// The chat (1:1 or group) a message of ours goes to.
    fn message_to_any(&self) -> Option<&ConversationId> {
        match self {
            Outbound::Send { conversation, message: Some(_), .. }
            | Outbound::SendImages { conversation, .. }
            | Outbound::SendGif { conversation, .. }
            | Outbound::SendSticker { conversation, .. } => Some(conversation),
            _ => None,
        }
    }

    /// The message row whose status this send tracks, with its chat and
    /// timestamp.
    fn tracked(&self) -> Option<(ConversationId, i64, u64)> {
        match self {
            Outbound::Send { conversation, message: Some(id), timestamp, .. }
            | Outbound::SendImages { conversation, message: id, timestamp, .. }
            | Outbound::SendSticker { conversation, message: id, timestamp, .. }
            | Outbound::SendGif { conversation, message: id, timestamp, .. } => Some((conversation.clone(), *id, *timestamp)),
            _ => None,
        }
    }
}

struct Actor {
    config: Config,
    db: AppDb,
    events: mpsc::UnboundedSender<Event>,
    store: SqliteStore,
    manager: RefCell<Option<SignalManager>>,
    self_aci: RefCell<String>,
    /// Display name cache: service id -> name (None = known to be unknown).
    names: RefCell<HashMap<String, Option<String>>>,
    /// Conversations whose title has been resolved this session.
    titled: RefCell<HashSet<ConversationId>>,
    /// Each group's revision when its details (members, admins, access) were
    /// last read, to read them again when a message names a newer one.
    group_revisions: RefCell<HashMap<ConversationId, u32>>,
    /// One group change at a time per group (`group_admin.rs`).
    group_locks: RefCell<HashMap<ConversationId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// Held while a group's stored state is read and written again.
    group_state_locks: RefCell<HashMap<ConversationId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// When each group was last fetched for its chat or details.
    group_fetches: RefCell<HashMap<ConversationId, std::time::Instant>>,
    /// Group identifier -> master key, for typing indicators.
    group_ids: RefCell<HashMap<Vec<u8>, [u8; 32]>>,
    profile_fetches: RefCell<HashSet<String>>,
    receiver: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Fetches release notes into the Signal chat.
    release_notes: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Shows megaphones; notified when they change.
    megaphones: RefCell<Option<tokio::task::JoinHandle<()>>>,
    megaphone_check: tokio::sync::Notify,
    /// Stories being sent, recipient by recipient.
    story_jobs: RefCell<HashMap<u64, stories::StoryJob>>,
    next_story_job: std::cell::Cell<u64>,
    /// When a group's send endorsements were last fetched without getting
    /// usable ones, so a failing fetch isn't repeated on every send.
    endorsement_misses: RefCell<HashMap<ConversationId, std::time::Instant>>,
    /// Lets stories go after a day; notified when one arrives.
    story_expiry_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    story_expiry: tokio::sync::Notify,
    /// Early events being applied now (`early.rs`), by row.
    early_replaying: RefCell<HashSet<i64>>,
    /// Early events have been tried again this session.
    early_retried: RefCell<bool>,
    /// Erases view-once messages left unviewed; notified when one arrives.
    view_once_expiry_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    view_once_expiry: tokio::sync::Notify,
    /// RingRTC, once a call needs it.
    calls: RefCell<Option<signal_calls::Calls>>,
    /// The call in progress.
    call: RefCell<Option<calls::ActiveCall>>,
    /// The group call we're in, or being rung for.
    group_call: RefCell<Option<group_calls::GroupCall>>,
    /// Lobby peeks awaiting their answer, by request id.
    peeks: RefCell<HashMap<u32, ConversationId>>,
    /// Today's call link credential (`call_links.rs`).
    link_auth: RefCell<Option<crate::call_links::AuthCredential>>,
    /// The calling server's call link API.
    link_client: std::cell::OnceCell<signal_calls::LinkClient>,
    /// Call links to read from the calling server, and whether a read is
    /// running (`call_links.rs`).
    link_refresh_queue: RefCell<Vec<String>>,
    link_refresh_running: std::cell::Cell<bool>,
    next_peek: std::cell::Cell<u32>,
    /// The microphone, speaker and camera chosen in Settings.
    call_choices: RefCell<crate::CallDevices>,
    /// RingRTC's events, for `call_loop`.
    call_events: mpsc::UnboundedSender<signal_calls::CallEvent>,
    /// Call messages to send, in RingRTC's order (an offer before its ICE).
    call_sends: mpsc::UnboundedSender<(Aci, presage::proto::CallMessage)>,
    /// The call's video frames, shared with the UI.
    video: crate::CallVideo,
    /// Identity keys (hex) seen this session, ahead of `SETTING_IDENTITY`.
    identities_seen: RefCell<HashMap<String, String>>,
    /// A later batch of profile photo checks is scheduled.
    avatar_batch_pending: std::cell::Cell<bool>,
    /// The history transfer after linking, while it can still be skipped
    /// (taken once the import starts).
    history: RefCell<Option<tokio::task::JoinHandle<()>>>,
    outbound: mpsc::UnboundedSender<Outbound>,
    linking: RefCell<bool>,
    /// Storage Service contacts have been read this session.
    storage_synced: RefCell<bool>,
    /// The account record's settings have been read from Storage Service
    /// this session.
    account_record_read: std::cell::Cell<bool>,
    /// Long texts cut off last time have been tried again this session.
    long_texts_resumed: RefCell<bool>,
    /// Long texts with a retry scheduled, and its time (only the newest
    /// counts).
    long_text_retries: RefCell<HashMap<i64, i64>>,
    /// Our poll votes still sending when the app last quit were queued again.
    poll_votes_resumed: RefCell<bool>,
    /// Reads attachments from the backup (media) tier.
    backup_media: crate::backup_media::BackupMedia,
    /// Held across a story list edit's read and write of the saved lists,
    /// so two edits at once don't undo each other.
    story_lists_edit: tokio::sync::Mutex<()>,
    /// Wakes the disappearing-message sweeper when a countdown starts.
    expiry: tokio::sync::Notify,
    /// The last timestamp we sent with: each send gets a later one, since
    /// a timestamp names a message (two in one millisecond would collide).
    last_sent_at: std::cell::Cell<i64>,
    /// When the window's activation last fetched the remote config.
    remote_config_at: std::cell::Cell<Option<Instant>>,
    /// Our key transparency self check while it runs.
    kt_self: RefCell<Option<key_transparency::SelfCheck>>,
    /// Wakes the self check's scheduler when its time moves.
    kt_schedule: tokio::sync::Notify,
    /// A contact's key transparency check, while it runs.
    kt_contact: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Runs the weekly self check.
    kt_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// When each person's profile was last fetched for being shown
    /// (`Command::LoadProfile`), to fetch it at most every five minutes.
    profile_shown: RefCell<HashMap<String, Instant>>,
    /// Badge images are being downloaded (`profile.rs`).
    badge_downloads: std::cell::Cell<bool>,
    /// The username reservation in flight; a newer one aborts it.
    username_reservation: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Held across a profile or username write, so two don't interleave.
    profile_edit: tokio::sync::Mutex<()>,
}

pub(crate) async fn run(
    config: Config,
    db: AppDb,
    video: crate::CallVideo,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) {
    let store = match open_presage_store(&config).await {
        Ok(store) => {
            // presage can't close its pool. If the pool were dropped, sqlx
            // would close connections on its worker threads, racing
            // SQLCipher's exit-time shutdown (which frees its memory pool) and
            // crashing on quit. Keep it open for the life of the process.
            std::mem::forget(store.clone());
            store
        }
        Err(error) => {
            let _ = events.unbounded_send(Event::Error(format!("failed to open signal.db: {error}")));
            return;
        }
    };

    let (outbound, outbound_rx) = mpsc::unbounded();
    let (call_events, call_events_rx) = mpsc::unbounded();
    let (call_sends, call_sends_rx) = mpsc::unbounded();
    let actor = Rc::new(Actor {
        config,
        db,
        events,
        store,
        manager: RefCell::new(None),
        self_aci: RefCell::new(String::new()),
        names: RefCell::default(),
        titled: RefCell::default(),
        group_revisions: RefCell::default(),
        group_locks: RefCell::default(),
        group_state_locks: RefCell::default(),
        group_fetches: RefCell::default(),
        group_ids: RefCell::default(),
        profile_fetches: RefCell::default(),
        receiver: RefCell::new(None),
        release_notes: RefCell::new(None),
        megaphones: RefCell::new(None),
        megaphone_check: tokio::sync::Notify::new(),
        story_expiry_task: RefCell::new(None),
        story_jobs: RefCell::new(HashMap::new()),
        next_story_job: std::cell::Cell::new(0),
        endorsement_misses: RefCell::default(),
        story_expiry: tokio::sync::Notify::new(),
        early_replaying: RefCell::default(),
        early_retried: RefCell::new(false),
        view_once_expiry_task: RefCell::new(None),
        view_once_expiry: tokio::sync::Notify::new(),
        identities_seen: RefCell::default(),
        calls: RefCell::new(None),
        call: RefCell::new(None),
        group_call: RefCell::new(None),
        peeks: RefCell::default(),
        link_auth: RefCell::new(None),
        link_client: std::cell::OnceCell::new(),
        link_refresh_queue: RefCell::default(),
        link_refresh_running: std::cell::Cell::new(false),
        next_peek: std::cell::Cell::new(0),
        call_choices: RefCell::default(),
        call_events,
        call_sends,
        video,
        avatar_batch_pending: std::cell::Cell::new(false),
        history: RefCell::new(None),
        outbound,
        linking: RefCell::new(false),
        storage_synced: RefCell::new(false),
        account_record_read: Default::default(),
        long_texts_resumed: RefCell::new(false),
        long_text_retries: RefCell::default(),
        poll_votes_resumed: RefCell::new(false),
        backup_media: Default::default(),
        story_lists_edit: tokio::sync::Mutex::new(()),
        expiry: tokio::sync::Notify::new(),
        last_sent_at: std::cell::Cell::new(0),
        remote_config_at: std::cell::Cell::new(None),
        kt_self: RefCell::new(None),
        kt_schedule: tokio::sync::Notify::new(),
        kt_contact: RefCell::new(None),
        profile_shown: RefCell::default(),
        badge_downloads: std::cell::Cell::new(false),
        username_reservation: RefCell::new(None),
        profile_edit: tokio::sync::Mutex::new(()),
        kt_task: RefCell::new(None),
    });

    tokio::task::spawn_local(actor.clone().outbound_loop(outbound_rx));
    tokio::task::spawn_local(actor.clone().expiry_loop());
    tokio::task::spawn_local(actor.clone().call_loop(call_events_rx));
    tokio::task::spawn_local(actor.clone().call_send_loop(call_sends_rx));
    actor.load_call_choices().await;
    actor.load_remote_config().await;
    crate::auto_download::load(&actor.db).await;

    if actor.store.is_registered().await {
        match Manager::load_registered(actor.store.clone()).await {
            Ok(manager) => actor.start_session(manager, false).await,
            Err(error) => {
                warn!(%error, "failed to load registration");
                actor.emit(Event::Error(format!("failed to load account: {error}")));
                actor.emit(Event::NeedsLinking);
            }
        }
    } else {
        actor.emit(Event::NeedsLinking);
    }

    while let Some(command) = commands.next().await {
        actor.clone().handle(command);
    }
    debug!("command channel closed; backend shutting down");
    if let Some(task) = actor.receiver.borrow_mut().take() {
        task.abort();
    }
}

async fn open_presage_store(config: &Config) -> anyhow::Result<SqliteStore> {
    let options = SqliteConnectOptions::new()
        .filename(config.data_dir.join("signal.db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let options = signal_store::keyed(options, &config.db_key);
    Ok(SqliteStore::open_with_options(options, OnNewIdentity::Trust).await?)
}

impl Actor {
    fn emit(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    fn error(&self, context: &str, error: impl std::fmt::Display) {
        warn!(%error, "{context}");
        self.emit(Event::Error(format!("{context}: {error}")));
    }

    fn manager(&self) -> Option<SignalManager> {
        self.manager.borrow().clone()
    }

    /// A timestamp for something we send: now, or just after the last one.
    fn next_timestamp(&self) -> i64 {
        let timestamp = now_millis().max(self.last_sent_at.get() + 1);
        self.last_sent_at.set(timestamp);
        timestamp
    }

    fn self_aci(&self) -> String {
        self.self_aci.borrow().clone()
    }

    async fn start_session(self: &Rc<Self>, manager: SignalManager, just_linked: bool) {
        let data = manager.registration_data();
        let aci = data.service_ids.aci().service_id_string();
        let phone = Some(data.phone_number.to_string());
        *self.self_aci.borrow_mut() = aci.clone();
        *self.manager.borrow_mut() = Some(manager.clone());

        // No viewer is open yet: copies of view-once media left from last
        // time go.
        self.clear_view_once_copies().await;
        // Note to Self is always present, as in Signal Desktop.
        let note = ConversationId::direct(&aci);
        if let Err(error) = self
            .db
            .upsert_conversation(
                &note,
                ConversationUpdate {
                    kind: Some(ConversationKind::NoteToSelf),
                    title: Some("Note to Self".into()),
                    phone: phone.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            self.error("failed to create Note to Self", error);
        }
        self.titled.borrow_mut().insert(note);

        let phone_for_notes = phone.clone();
        self.emit(Event::Ready { phone, aci });
        self.refresh_titles().await;

        if just_linked {
            let mut m = manager.clone();
            let this = self.clone();
            tokio::task::spawn_local(async move {
                if let Err(error) = m.request_contacts().await {
                    this.error("failed to request contacts", error);
                }
            });
            if manager.registration_data().ephemeral_backup_key().is_some() {
                let _ = self.db.set_setting(SETTING_HISTORY_PENDING, "1").await;
            }
            // Desktop takes the phone's read receipts setting when linked;
            // Storage Service has the rest once read.
            if let Some(on) = manager.registration_data().read_receipts() {
                self.save_account_settings(crate::AccountSettings::flag(AccountSetting::ReadReceipts, on)).await;
            }
            // Desktop keeps the media root key from linking
            // (`AccountManager`'s `backupMediaRootKey`).
            if let Some(key) = manager.registration_data().media_root_backup_key() {
                self.set_media_root_key(Some(key)).await;
            }
            // Desktop's device age, for a donation megaphone's condition.
            let _ = self.db.set_setting(SETTING_DEVICE_CREATED, &now_millis().to_string()).await;
            self.kt_linked().await;
        }
        // The history transfer, also resumed if the app quit during it.
        let pending = self.db.setting(SETTING_HISTORY_PENDING).await.ok().flatten().is_some_and(|v| v == "1");
        if pending && let Some(key) = manager.registration_data().ephemeral_backup_key() {
            let task = tokio::task::spawn_local(self.clone().transfer_history(manager.clone(), key));
            if let Some(old) = self.history.borrow_mut().replace(task) {
                old.abort();
            }
        }

        let task = tokio::task::spawn_local(self.clone().receive_loop(manager));
        if let Some(old) = self.receiver.borrow_mut().replace(task) {
            old.abort();
        }
        // Release notes for the Signal chat, as Desktop fetches them.
        let task = tokio::task::spawn_local(self.clone().release_notes_loop(phone_for_notes));
        if let Some(old) = self.release_notes.borrow_mut().replace(task) {
            old.abort();
        }
        let task = tokio::task::spawn_local(self.clone().megaphone_loop());
        if let Some(old) = self.megaphones.borrow_mut().replace(task) {
            old.abort();
        }
        let task = tokio::task::spawn_local(self.clone().story_expiry_loop());
        if let Some(old) = self.story_expiry_task.borrow_mut().replace(task) {
            old.abort();
        }
        let task = tokio::task::spawn_local(self.clone().view_once_expiry_loop());
        if let Some(old) = self.view_once_expiry_task.borrow_mut().replace(task) {
            old.abort();
        }
        let task = tokio::task::spawn_local(self.clone().key_transparency_loop());
        if let Some(old) = self.kt_task.borrow_mut().replace(task) {
            old.abort();
        }
    }

    /// Link and sync: waits for the phone's upload, then imports it.
    async fn transfer_history(self: Rc<Self>, manager: SignalManager, key: [u8; 32]) {
        use crate::HistoryProgress;
        self.emit(Event::History(HistoryProgress::Waiting));
        let result = async {
            let (cdn, archive) = match crate::history::wait_for_archive(&manager).await? {
                crate::history::Archive::Ready { cdn, key } => (cdn, key),
                crate::history::Archive::Skipped => return anyhow::Ok(None),
            };
            let mut every = Every::new(PROGRESS_INTERVAL);
            let data = crate::history::download(&manager, cdn, &archive, |done, total| {
                if every.ready() || Some(done) == total {
                    self.emit(Event::History(HistoryProgress::Downloading { done, total }));
                }
            })
            .await?;
            // Past the point of skipping: a partial import would stay.
            self.history.borrow_mut().take();
            let aci = manager.registration_data().service_ids.aci();
            let plain = tokio::task::spawn_blocking(move || crate::history::decrypt(data, key, aci)).await??;
            let total = plain.len() as u64;
            self.emit(Event::History(HistoryProgress::Importing { done: 0, total }));
            let mut every = Every::new(PROGRESS_INTERVAL);
            let mut imported = crate::history::import(&self.db, &plain, &aci.service_id_string(), |done| {
                if every.ready() {
                    self.emit(Event::History(HistoryProgress::Importing { done, total }));
                }
            })
            .await?;
            // The phone's settings as of the backup, unless Storage Service
            // (newer, and what Desktop reads after the backup) came first.
            if let Some(settings) = imported.settings.take()
                && !self.account_record_read.get()
            {
                self.save_account_settings(settings).await;
            }
            // Transferred view-once messages may be old enough to erase.
            self.view_once_expiry.notify_one();
            let packs = std::mem::take(&mut imported.sticker_packs);
            if !packs.is_empty() {
                tokio::task::spawn_local(self.clone().install_sticker_packs(manager.clone(), packs));
            }
            let downloads = std::mem::take(&mut imported.downloads);
            if !downloads.is_empty() {
                tokio::task::spawn_local(self.clone().download_transferred(manager.clone(), downloads));
            }
            // What came for transferred messages before they were here.
            self.retry_early_events(&manager, false).await;
            let long_texts = std::mem::take(&mut imported.long_texts);
            if !long_texts.is_empty() {
                let (this, manager) = (self.clone(), manager.clone());
                tokio::task::spawn_local(async move {
                    // One at a time: they're small, and there can be many.
                    for id in long_texts {
                        this.download_long_text(&manager, id, LongTextTry::First).await;
                    }
                });
            }
            anyhow::Ok(Some(imported))
        }
        .await;
        let progress = match result {
            Ok(Some(imported)) => HistoryProgress::Done { chats: imported.chats, messages: imported.messages },
            Ok(None) => HistoryProgress::Skipped,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "message history transfer failed");
                HistoryProgress::Failed(format!("{error:#}"))
            }
        };
        self.history.borrow_mut().take();
        self.finish_history(progress).await;
    }

    async fn finish_history(self: &Rc<Self>, progress: crate::HistoryProgress) {
        // Done either way; a failed transfer isn't retried forever.
        let _ = self.db.set_setting(SETTING_HISTORY_PENDING, "0").await;
        self.titled.borrow_mut().clear();
        self.refresh_titles().await;
        self.emit(Event::ConversationsChanged);
        self.emit(Event::History(progress));
        // The transfer usually adds chats after this session's first photo
        // sync ran.
        if let Some(manager) = self.manager() {
            self.sync_avatars(manager).await;
        }
    }

    async fn skip_history(self: &Rc<Self>) {
        let task = self.history.borrow_mut().take();
        if let Some(task) = task {
            task.abort();
            self.finish_history(crate::HistoryProgress::Skipped).await;
        }
    }

    fn handle(self: Rc<Self>, command: Command) {
        tokio::task::spawn_local(async move {
            match command {
                Command::Link => self.link().await,
                Command::SendText { conversation, body, quote, preview, mentions, styles } => {
                    self.send_text(conversation, body, quote, preview, mentions, styles).await
                }
                Command::SearchGifs { query, offset } => {
                    let cache = self.config.data_dir.join("gif-cache");
                    let result = crate::search_gifs(&self.db, &cache, &query, offset).await;
                    self.emit(Event::Gifs { query, offset, result });
                }
                Command::SendGif { conversation, gif, quote } => self.send_gif(conversation, gif, quote).await,
                Command::LoadStickers => {
                    let packs = self.load_stickers().await;
                    self.emit(Event::Stickers(packs));
                }
                Command::SendSticker { conversation, pack, sticker, quote } => {
                    self.send_sticker(conversation, &pack, sticker, quote).await
                }
                Command::UploadStickerPack(pack) => self.upload_sticker_pack(pack).await,
                Command::FindRecipient { query } => {
                    let result = self.find_recipient(&query).await;
                    self.emit(Event::RecipientFound { query, result });
                }
                Command::FetchLinkPreview { url } => {
                    let preview = if self.setting_on(AccountSetting::LinkPreviews).await {
                        crate::link_preview::fetch(&url, self.image_optimization().await).await.unwrap_or_else(|error| {
                            debug!(error = %format!("{error:#}"), "no link preview");
                            None
                        })
                    } else {
                        None
                    };
                    self.emit(Event::LinkPreview { url, preview });
                }
                Command::SendImages { conversation, images, caption, quote, mentions, styles } => {
                    self.send_images(conversation, images, caption, quote, mentions, styles).await
                }
                Command::SendFile { conversation, file, caption, quote, mentions, styles } => {
                    self.send_file(conversation, file, caption, quote, mentions, styles).await
                }
                Command::React { message_id, emoji, remove } => {
                    self.react(message_id, emoji, remove).await
                }
                Command::SendPoll { conversation, question, options, allow_multiple } => {
                    self.send_poll(conversation, question, options, allow_multiple).await
                }
                Command::VotePoll { message_id, options } => self.vote_in_poll(message_id, options).await,
                Command::EndPoll { message_id } => self.end_poll(message_id).await,
                Command::MarkRead { conversation } => self.mark_read(conversation).await,
                Command::Typing { conversation, started } => self.typing(conversation, started).await,
                Command::DeleteForMe { message_ids } => self.delete_for_me(message_ids).await,
                Command::RetrySend { message_id } => self.retry_send(message_id).await,
                Command::EditMessage { message_id, body, mentions, styles } => {
                    self.edit_message(message_id, body, mentions, styles).await
                }
                Command::PinMessage { message_id, duration } => self.set_pin(message_id, true, duration).await,
                Command::UnpinMessage { message_id } => self.set_pin(message_id, false, None).await,
                Command::Forward { message_ids, conversations } => self.forward(message_ids, conversations).await,
                Command::DeleteConversation { conversation } => self.delete_conversation(conversation).await,
                Command::AnswerMessageRequest { conversation, answer } => {
                    self.answer_message_request(conversation, answer).await
                }
                Command::DeleteForEveryone { message_id } => self.delete_for_everyone(message_id).await,
                Command::FetchAttachment { message_id, index } => {
                    self.fetch_attachment(message_id, index).await
                }
                Command::OpenViewOnce { message_id } => self.open_view_once(message_id).await,
                Command::SendViewOnce { conversation, media } => self.send_view_once(conversation, media).await,
                Command::FetchLongText { message_id } => {
                    if let Some(manager) = self.manager() {
                        self.download_long_text(&manager, message_id, LongTextTry::Asked).await;
                    }
                }
                Command::SetPinned { conversation, pinned } => self.set_pinned(conversation, pinned).await,
                Command::SetChat { conversation, change } => self.set_chat(conversation, change).await,
                Command::SetNickname { conversation, nickname } => self.set_nickname(conversation, nickname).await,
                Command::SetExpireTimer { conversation, seconds } => self.set_expire_timer(conversation, seconds).await,
                Command::UpdateGroup { conversation, title, description } => {
                    self.update_group(conversation, title, description).await
                }
                Command::LeaveGroup { conversation } => self.leave_group(conversation).await,
                Command::CreateGroup { title, members, avatar, expire_timer } => {
                    self.create_group(title, members, avatar, expire_timer).await
                }
                Command::SetGroupAvatar { conversation, image } => self.set_group_avatar(conversation, image).await,
                Command::RemoveFromGroup { conversation, service_id } => self.remove_from_group(conversation, service_id).await,
                Command::SetGroupAdmin { conversation, member, admin } => self.set_group_admin(conversation, member, admin).await,
                Command::SetGroupPermission { conversation, permission } => {
                    self.set_group_permission(conversation, permission).await
                }
                Command::ChangeGroupLink { conversation, change } => self.change_group_link(conversation, change).await,
                Command::AnswerJoinRequest { conversation, member, approve } => {
                    self.answer_join_request(conversation, member, approve).await
                }
                Command::RevokeGroupInvites { conversation, invitees } => self.revoke_group_invites(conversation, invitees).await,
                Command::CancelJoinRequest { conversation } => self.cancel_join_request(conversation).await,
                Command::OpenGroupLink { url } => self.open_group_link(url).await,
                Command::JoinGroupLink { url } => self.join_group_link(url).await,
                Command::RefreshGroup { conversation } => self.refresh_group(conversation).await,
                Command::SetMemberLabel { conversation, emoji, text } => self.set_member_label(conversation, emoji, text).await,
                Command::LoadSafetyNumber { conversation } => self.emit_safety_number(conversation).await,
                Command::CheckKeyTransparency { conversation } => self.check_key_transparency(conversation),
                Command::CancelKeyTransparency => self.cancel_key_transparency(),
                Command::SetKeyTransparencyDisabled(disabled) => self.set_key_transparency_disabled(disabled).await,
                Command::SetVerified { conversation, verified } => self.set_verified(conversation, verified).await,
                Command::PressMegaphone { id, cta } => self.press_megaphone(id, cta).await,
                Command::ViewStory { id } => self.view_story(id).await,
                Command::FetchStory { id } => self.fetch_story(id).await,
                Command::ReplyToStory { id, body, emoji } => self.reply_to_story(id, body, emoji).await,
                Command::PostStory { story, targets } => {
                    let this = self.clone();
                    tokio::task::spawn_local(async move {
                        let groups: Vec<ConversationId> = targets
                            .iter()
                            .filter_map(|t| match t {
                                crate::StoryTarget::Group(group) => Some(group.clone()),
                                _ => None,
                            })
                            .collect();
                        this.post_story(story, targets).await;
                        // Desktop's `sendStoryMessage`: a group posted to
                        // stays on the list (`StorySendMode.Always`).
                        if !groups.is_empty() {
                            this.set_story_send_mode(groups, signal_store::StorySendMode::Always).await;
                        }
                    });
                }
                Command::SaveStoryList(list) => self.save_story_list(list).await,
                Command::DeleteStoryList { id } => self.delete_story_list(id).await,
                Command::SetStoriesDisabled(disabled) => self.set_stories_disabled(disabled).await,
                Command::SetStoryViewReceipts(on) => self.set_story_view_receipts(on).await,
                Command::SetAccountSetting { setting, on } => self.set_account_setting(setting, on).await,
                Command::SetUniversalExpireTimer(seconds) => self.set_universal_expire_timer(seconds).await,
                Command::SetPhoneNumberSharing(everybody) => self.set_phone_number_sharing(everybody).await,
                Command::SetPhoneNumberDiscoverable(on) => self.set_phone_number_discoverable(on).await,
                Command::SyncContacts => self.request_syncs(),
                Command::SetAutoDownload(choices) => self.set_auto_download(choices).await,
                Command::StartCall { conversation, video } if conversation.as_call_link().is_some() => {
                    self.start_link_call(conversation, video).await
                }
                Command::SetStorySendMode { groups, mode } => self.set_story_send_mode(groups, mode).await,
                Command::DeleteStory { id } => match self.db.story(id).await {
                    Ok(Some(story)) if story.author.is_none() => {
                        self.delete_story_for_everyone(&story, None).await;
                        self.emit(Event::StoriesChanged);
                    }
                    Ok(_) => {}
                    Err(error) => self.error("couldn't delete the story", error),
                },
                Command::DeleteStoryReply { story, author, sent_at, for_everyone } => {
                    self.delete_story_reply(story, author, sent_at, for_everyone).await
                }
                Command::StartCall { conversation, video } => self.start_call(conversation, video),
                Command::AcceptCall { video } => self.accept_call(video),
                Command::DeclineCall | Command::HangUp => self.hang_up().await,
                Command::SetCallAudio(enabled) => self.set_call_audio(enabled),
                Command::SetCallVideo(enabled) => self.set_call_video(enabled),
                Command::LoadCallDevices => self.load_call_devices().await,
                Command::LoadShareSources => self.load_share_sources().await,
                Command::StartSharing { source } => self.start_sharing(source),
                Command::StopSharing => self.stop_sharing(),
                Command::SetCallDevice { kind, id } => self.set_call_device(kind, id).await,
                Command::PeekGroupCall { conversation } if conversation.as_call_link().is_some() => {
                    self.peek_call_link(conversation).await
                }
                Command::PeekGroupCall { conversation } => self.peek_group_call(conversation).await,
                Command::MarkCallsRead { conversation } => self.mark_calls_read(conversation).await,
                Command::CallReact { emoji } => self.call_react(emoji),
                Command::RaiseHand(raise) => self.raise_hand(raise),
                Command::RemoteMute { demux_id } => self.remote_mute(demux_id),
                Command::ShowGroupVideo { visible, main } => self.show_group_video(visible, main),
                Command::ClearCallHistory => self.clear_call_history().await,
                Command::CreateCallLink => self.create_call_link().await,
                Command::UpdateCallLink { room_id, name, restrictions } => self.update_call_link(room_id, name, restrictions).await,
                Command::DeleteCallLink { room_id } => self.delete_call_link(room_id).await,
                Command::OpenCallLink { url } => self.open_call_link(url).await,
                Command::ApproveCallJoin { aci, approve } => self.approve_call_join(aci, approve),
                Command::RemoveFromCall { demux_id } => self.remove_from_call(demux_id),
                Command::Unlink => self.unlink().await,
                Command::SkipHistory => self.skip_history().await,
                Command::AddGroupMembers { conversation, members } => self.add_group_members(conversation, members).await,
                Command::EndGroup { conversation } => self.end_group(conversation).await,
                Command::RefreshRemoteConfig => self.maybe_refresh_remote_config().await,
                Command::LoadProfile { service_id } => self.load_profile(service_id).await,
                Command::SetProfile(edit) => self.set_profile(edit).await,
                Command::ReserveUsername { nickname, discriminator } => self.reserve_username(nickname, discriminator),
                Command::ConfirmUsername(reservation) => self.confirm_username(reservation).await,
                Command::DeleteUsername => self.delete_username().await,
                Command::ResetUsernameLink => self.reset_username_link().await,
                Command::SetUsernameLinkColor(color) => self.set_username_link_color(color).await,
            }
        });
    }

    // ---------------------------------------------------------------- linking

    async fn link(self: Rc<Self>) {
        if self.linking.replace(true) {
            debug!("link already in progress");
            return;
        }
        if let Some(task) = self.receiver.borrow_mut().take() {
            task.abort();
        }
        self.stop_release_notes();
        self.manager.borrow_mut().take();

        let (url_tx, url_rx) = oneshot::channel();
        let this = self.clone();
        let (result, ()) = future::join(
            Manager::link_secondary_device(
                self.store.clone(),
                SignalServers::Production,
                self.config.device_name.clone(),
                url_tx,
            ),
            async move {
                if let Ok(url) = url_rx.await {
                    this.emit(Event::LinkUrl(url));
                }
            },
        )
        .await;
        *self.linking.borrow_mut() = false;

        match result {
            Ok(manager) => {
                info!("linked as secondary device");
                // A fresh link may be a different account: start from a clean slate.
                self.identities_seen.borrow_mut().clear();
        if let Err(error) = self.db.clear().await {
                    self.error("failed to reset app database", error);
                }
                self.titled.borrow_mut().clear();
                self.names.borrow_mut().clear();
                self.start_session(manager, true).await;
                self.emit(Event::ConversationsChanged);
            }
            Err(error) => {
                warn!(%error, "linking failed");
                self.emit(Event::LinkFailed(error.to_string()));
            }
        }
    }

    async fn unlink(self: Rc<Self>) {
        // Any call ends with the account (RingRTC closes when dropped).
        self.call.borrow_mut().take();
        self.group_call.borrow_mut().take();
        let calls = self.calls.borrow_mut().take();
        drop(calls);
        if let Some(task) = self.receiver.borrow_mut().take() {
            task.abort();
        }
        if let Some(task) = self.history.borrow_mut().take() {
            task.abort();
        }
        // They'd make the Signal chat again in the wiped database.
        self.stop_release_notes();
        let manager = self.manager.borrow_mut().take();
        if let Some(manager) = manager
            && let Err(error) = manager.unlink_self().await
        {
            // Still wipe local state: the user asked to be logged out.
            warn!(%error, "server-side unlink failed");
        }
        self.wipe().await;
        self.emit(Event::Unlinked);
        self.emit(Event::NeedsLinking);
    }

    async fn wipe(&self) {
        let mut store = self.store.clone();
        if let Err(error) = store.clear_contents().await {
            warn!(%error, "failed to clear presage contents");
        }
        if let Err(error) = store.clear_registration().await {
            warn!(%error, "failed to clear presage registration");
        }
        self.identities_seen.borrow_mut().clear();
        if let Err(error) = self.db.clear().await {
            warn!(%error, "failed to clear app database");
        }
        // Downloaded and sent media are files, unencrypted: they go too.
        for dir in ["avatars", "attachments", "stickers", "gif-cache", settings::CONTACT_AVATARS_DIR] {
            let _ = std::fs::remove_dir_all(self.config.data_dir.join(dir));
        }
        let _ = std::fs::create_dir_all(self.config.data_dir.join("attachments"));
        self.titled.borrow_mut().clear();
        self.names.borrow_mut().clear();
        self.self_aci.borrow_mut().clear();
        self.storage_synced.replace(false);
        crate::remote_config::set_current(Default::default());
        crate::auto_download::set_current(Default::default());
        self.remote_config_at.set(None);
    }

    // -------------------------------------------------------------- receiving

    async fn receive_loop(self: Rc<Self>, mut manager: SignalManager) {
        let mut backoff = Duration::from_secs(1);
        let mut caught_up = false;
        let mut catch_up_first = None;
        let mut catch_up_every = Every::new(PROGRESS_INTERVAL);
        // The remote config before the queue, as Desktop fetches it.
        self.first_remote_config(&manager).await;
        loop {
            self.emit(Event::Connection(ConnectionState::Connecting));
            let started = Instant::now();
            match manager.receive_messages().await {
                Ok(stream) => {
                    self.emit(Event::Connection(ConnectionState::Syncing));
                    pin_mut!(stream);
                    while let Some(received) = stream.next().await {
                        match &received {
                            // Desktop's "Loading messages from …" screen, for the first catch-up only.
                            Received::Content(content) if !caught_up => {
                                let at = content.metadata.server_timestamp.timestamp_millis();
                                let first = *catch_up_first.get_or_insert(at);
                                if catch_up_every.ready() {
                                    self.emit(Event::CatchingUp { first, latest: at.max(first) });
                                }
                            }
                            Received::QueueEmpty => caught_up = true,
                            _ => {}
                        }
                        self.on_received(&manager, received).await;
                    }
                    info!("message stream ended");
                }
                Err(presage::Error::ServiceError(ServiceError::Unauthorized)) => {
                    warn!("server rejected our credentials; device was unlinked");
                    self.manager.borrow_mut().take();
                    self.wipe().await;
                    self.emit(Event::Connection(ConnectionState::Offline));
                    self.emit(Event::Unlinked);
                    self.emit(Event::NeedsLinking);
                    return;
                }
                Err(error) => warn!(%error, "failed to open message stream"),
            }
            self.emit(Event::Connection(ConnectionState::Offline));
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            debug!(?backoff, "reconnecting");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    async fn on_received(self: &Rc<Self>, manager: &SignalManager, received: Received) {
        match received {
            Received::QueueEmpty => {
                self.emit(Event::Connection(ConnectionState::Online));
                self.refresh_titles().await;
                // As Desktop reads its caches again at startup, once
                // what was waiting on the server is in.
                if !self.early_retried.replace(true) {
                    self.retry_early_events(manager, true).await;
                }
                if !self.long_texts_resumed.replace(true) {
                    tokio::task::spawn_local(self.clone().resume_long_texts(manager.clone()));
                }
                if !self.poll_votes_resumed.replace(true) {
                    self.resume_poll_votes().await;
                }
                if !self.storage_synced.replace(true) {
                    self.spawn_storage_sync(manager);
                    self.request_keys_if_missing().await;
                }
            }
            Received::Contacts => {
                info!("contacts synchronized");
                self.names.borrow_mut().clear();
                self.titled.borrow_mut().clear();
                self.refresh_titles().await;
                self.backfill_author_names().await;
                self.save_contact_avatars().await;
                let _ = self.db.set_setting(crate::SETTING_CONTACTS_SYNCED_AT, &now_millis().to_string()).await;
                self.emit(Event::ContactsSynced);
            }
            Received::DecryptionError(sender) => {
                warn!(sender = %sender.service_id_string(), "failed to decrypt message");
            }
            Received::Content(content) => {
                let self_aci = self.self_aci();
                let sender = content.metadata.sender.service_id_string();
                if sender != self_aci {
                    self.check_identity(&sender).await;
                    self.keep_reporting_token(&sender, content.metadata.server_guid).await;
                }
                // The phone's keys (Desktop's `onKeysSync`): presage keeps
                // the account entropy pool; the media root key is ours.
                if sender == self_aci
                    && let ContentBody::SynchronizeMessage(SyncMessage { content: Some(sync_message::Content::Keys(keys)), .. }) =
                        &content.body
                {
                    let key = keys.media_root_backup_key.as_deref().and_then(|k| <[u8; 32]>::try_from(k).ok());
                    self.set_media_root_key(key).await;
                }
                if let ContentBody::CallMessage(call) = &content.body {
                    // Nothing from someone you blocked, calls included.
                    if sender != self_aci && self.is_blocked(&ConversationId::direct(&sender)).await {
                        return;
                    }
                    let device = u32::from(content.metadata.sender_device);
                    let at = content.metadata.server_timestamp.timestamp_millis();
                    self.on_call_message(&content.metadata.sender, device, at, call).await;
                    return;
                }
                if let Some((key, change, at)) = mapping::group_change_of(&content) {
                    self.apply_group_change(key, &change, at).await;
                }
                // A newer revision can come without the change itself (the
                // field is optional). presage has fetched the group again by
                // now, so read its admins and access again too.
                if let Some((key, revision)) = mapping::group_revision_of(&content) {
                    let conversation = ConversationId::group(&key);
                    let stale = self.group_revisions.borrow().get(&conversation).is_some_and(|seen| *seen < revision);
                    if stale {
                        self.titled.borrow_mut().remove(&conversation);
                        self.ensure_title(&conversation).await;
                        self.emit(Event::ConversationsChanged);
                        // presage's copy lacks announcement-only and bans:
                        // fetch those too (at most every few minutes).
                        tokio::task::spawn_local(self.clone().refresh_group(conversation));
                    }
                }
                // Like Desktop, nothing from someone you blocked gets through.
                if sender != self_aci && self.is_blocked(&ConversationId::direct(&sender)).await {
                    return;
                }
                // A one-to-one story reply, like a story, only from someone
                // we share our profile with (Desktop's `MessageReceiver`).
                if let ContentBody::DataMessage(dm) = &content.body
                    && dm.story_context.is_some()
                    && dm.group_v2.is_none()
                    && sender != self_aci
                    && !self.takes_story_from(&sender, None).await
                {
                    debug!("story reply from someone we don't share our profile with");
                    return;
                }
                let ctx = MapContext { self_aci: &self_aci, now: now_millis() };
                for mut action in mapping::map_content(&content, &ctx) {
                    // Theirs keep the server's GUID, for a spam report.
                    if let Action::Insert { message, server_guid, .. } = &mut action
                        && message.direction == Direction::Incoming
                    {
                        *server_guid = content.metadata.server_guid.map(|g| g.to_string());
                    }
                    // It came by sealed sender (Desktop's
                    // `unidentifiedDeliveryReceived`).
                    if let Action::Insert { message, sealed_sender, .. } = &mut action
                        && message.direction == Direction::Incoming
                        && content.metadata.unidentified_sender
                    {
                        sealed_sender.extend(message.author.clone());
                    }
                    // Nor from a blocked group (our own sync messages still apply).
                    if let Action::Insert { message, .. } = &action
                        && message.direction == Direction::Incoming
                        && self.is_blocked(&message.conversation_id).await
                    {
                        continue;
                    }
                    // Nor any message in an ended group (Desktop's
                    // `handleDataMessage`; the change that ended it was
                    // applied above).
                    if let Action::Insert { message, .. } = &action
                        && message.conversation_id.as_group().is_some()
                        && self.is_ended(&message.conversation_id).await
                    {
                        info!("dropping a message for an ended group");
                        continue;
                    }
                    self.apply(manager, action).await;
                }
            }
        }
    }

    async fn apply(self: &Rc<Self>, manager: &SignalManager, action: Action) {
        match action {
            Action::Insert {
                mut message,
                mentions,
                styles,
                pointers,
                long_text,
                preview_image,
                timer_version,
                poll,
                view_once,
                server_guid,
                sealed_sender,
            } => {
                let mut spans = Vec::new();
                let mut style_spans = Vec::new();
                // Every 1:1 message carries the sender's timer (groups take
                // theirs from the group).
                if message.conversation_id.as_group().is_none() {
                    let by = message.author.clone();
                    let (conversation, seconds, at) = (message.conversation_id.clone(), message.expires_in, message.sent_at);
                    self.apply_timer(&conversation, seconds, timer_version, false, by, at).await;
                }
                if message.expires_in > 0 && message.direction == Direction::Outgoing {
                    self.expiry.notify_one();
                }
                self.ensure_title(&message.conversation_id).await;
                if let Some(author) = &message.author {
                    message.author_name = self.name_of(author).await;
                }
                if !mentions.is_empty() || !styles.is_empty() {
                    let rendered = self.render_body(&message.body, &mentions, &styles).await;
                    message.body = rendered.0;
                    spans = rendered.1;
                    style_spans = rendered.2;
                }
                if let Some(quote) = &mut message.quote {
                    quote.author_name = match &quote.author {
                        None => "You".into(),
                        Some(a) => self.name_of(a).await.unwrap_or_else(|| short_id(a)),
                    };
                    // A story reply shows the story's photo (Desktop's "Story"
                    // quote).
                    if quote.story {
                        if let Ok(Some(story)) = self.db.find_story(quote.author.as_deref(), quote.sent_at).await
                            && let signal_store::StoryContent::Media { attachment, .. } = &story.content
                            && attachment.content_type.starts_with("image/")
                        {
                            quote.thumbnail = attachment.local_path.clone();
                        }
                    } else
                    // Like Desktop, show the quoted message's photo or link
                    // preview image beside the quote when we have it.
                    if let Ok(Some(quoted)) = self.db.find_message_any(quote.author.as_deref(), quote.sent_at).await {
                        quote.thumbnail = quoted.thumbnail();
                        // Ours knows what the wire doesn't: a voice message,
                        // or a view-once message, which says only that
                        // (Desktop's `copyQuoteContentFromOriginal`).
                        if let Some(attachment) = quoted_attachment(&quoted) {
                            if attachment.view_once {
                                quote.text.clear();
                            }
                            quote.attachment = Some(attachment);
                        }
                    }
                }
                let conversation = message.conversation_id.clone();
                // What downloads on its own: Desktop's media auto-download
                // choices (Data usage).
                let downloadable: Vec<(usize, AttachmentPointer)> = pointers
                    .into_iter()
                    .enumerate()
                    .filter(|(ix, _)| message.attachments.get(*ix).is_some_and(downloads_whole))
                    .collect();
                // Re-deliveries are de-duplicated by the store; only notify once.
                let repeat = self
                    .db
                    .find_message(&message.conversation_id, message.author.as_deref(), message.sent_at)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                match self.db.insert_received(&message, view_once, server_guid.as_deref()).await {
                    Ok(id) => {
                        // A message request's media waits until it's
                        // accepted (Desktop's `queueAttachmentDownloads`).
                        let held = message.direction == Direction::Incoming && !self.is_accepted(&conversation).await;
                        if view_once == Some(signal_store::ViewOnce::Ready) {
                            self.view_once_expiry.notify_one();
                        }
                        // Sent from another of our devices: Desktop shares
                        // our profile with that chat from then on
                        // (`handleDataMessage`); that device saves it to
                        // Storage Service.
                        if message.direction == Direction::Outgoing
                            && message.conversation_id.as_direct().is_some()
                            && !repeat
                            && let Err(error) = self.db.set_profile_sharing(&message.conversation_id, true).await
                        {
                            warn!(error = %format!("{error:#}"), "failed to save profile sharing");
                        }
                        if !spans.is_empty() {
                            let _ = self.db.set_mentions(id, &spans).await;
                        }
                        if !style_spans.is_empty() {
                            let _ = self.db.set_styles(id, &style_spans).await;
                        }
                        if !repeat && !sealed_sender.is_empty() {
                            let _ = self.db.set_sealed_sender(id, &sealed_sender).await;
                        }
                        // Only its start came in the body; the rest follows
                        // (a re-delivery is already on its way).
                        if !repeat && let Some(pointer) = &long_text {
                            self.expect_long_text(manager, id, pointer, mentions.clone(), styles.clone(), held).await;
                        }
                        if let Some(poll) = &poll
                            && let Err(error) = self.db.set_poll(id, poll).await
                        {
                            self.error("failed to store poll", error);
                        }
                        // Deletes, edits, reactions, votes, receipts and
                        // syncs that came before it. One read on another
                        // device, or deleted, doesn't notify.
                        let early = self.apply_early_for(manager, message.author.as_deref(), message.sent_at).await;
                        if !repeat && !early.read && !early.deleted && message.direction == Direction::Incoming {
                            let me = self.self_aci();
                            // A quote of ours has no author (it's us).
                            let replies_to_me = message.quote.as_ref().is_some_and(|q| q.author.is_none());
                            self.emit(Event::Incoming {
                                conversation: conversation.clone(),
                                message_id: id,
                                mentions_me: spans.iter().any(|s| s.aci == me),
                                replies_to_me,
                            });
                        }
                        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                        self.emit(Event::ConversationsChanged);
                        if message.direction == Direction::Incoming
                            && let Some(author) = &message.author
                        {
                            // A message implies they stopped typing.
                            self.emit(Event::Typing {
                                conversation: conversation.clone(),
                                author: author.clone(),
                                started: false,
                            });
                        }
                        // A link preview's image is a photo (Desktop's `queuePreviews`).
                        if let Some(pointer) = preview_image.filter(|p| {
                            !held
                                && p.content_type.as_deref().is_some_and(|c| c.starts_with("image/"))
                                && crate::auto_download::current().photos
                                && p.size.is_some_and(|size| crate::remote_config().auto_downloads(u64::from(size)))
                        }) {
                            let this = self.clone();
                            let manager = manager.clone();
                            let conversation = conversation.clone();
                            tokio::task::spawn_local(async move {
                                this.download_preview_image(&manager, conversation, id, &pointer).await
                            });
                        }
                        for (index, pointer) in downloadable.into_iter().filter(|_| !held) {
                            let this = self.clone();
                            let manager = manager.clone();
                            let conversation = conversation.clone();
                            tokio::task::spawn_local(async move {
                                this.download(&manager, conversation, id, index, &pointer).await
                            });
                        }
                    }
                    Err(error) => self.error("failed to store message", error),
                }
            }
            Action::React { .. } | Action::Delete { .. } | Action::AdminDelete { .. } | Action::Edit { .. } => {
                self.modify(manager, action).await;
            }
            Action::Receipt { sent_at, status } => {
                // With read receipts off we don't see others' either:
                // Desktop drops read and viewed receipts
                // (`ts/messageModifiers/MessageReceipts.preload.ts`,
                // `shouldDropReceipt`). Stories' views count apart.
                if status == DeliveryStatus::Read && !self.setting_on(AccountSetting::ReadReceipts).await {
                    debug!("read receipts are off; dropping a read receipt");
                    return;
                }
                for ts in sent_at {
                    self.modify(manager, Action::Receipt { sent_at: vec![ts], status }).await;
                }
            }
            Action::Typing { sender, group_id, started } => {
                // Nor others' typing (Desktop's `onTyping`).
                if !self.setting_on(AccountSetting::TypingIndicators).await {
                    return;
                }
                let conversation = match group_id {
                    None => ConversationId::direct(&sender),
                    Some(id) => match self.group_for_id(manager, &id).await {
                        Some(key) => ConversationId::group(&key),
                        None => return,
                    },
                };
                // Desktop's `notifyTyping` drops them in ended groups.
                if self.is_ended(&conversation).await {
                    return;
                }
                self.emit(Event::Typing { conversation, author: sender, started });
            }
            Action::CallEvent(event) => self.on_call_event(event).await,
            Action::GroupCallEvent(event) => self.on_group_call_event(event).await,
            Action::CallLogEvent(event) => self.on_call_log_event(event).await,
            Action::CallLinkUpdate { root_key, admin_key } => self.on_call_link_update(root_key, admin_key).await,
            Action::AdhocCallEvent(event) => self.on_adhoc_call_event(event).await,
            Action::GroupCallStarted { conversation, by, era, at } => {
                // (Not from a blocked group.)
                if by.is_none() || !self.is_blocked(&conversation).await {
                    self.on_group_call_started(conversation, by, era, at).await
                }
            }
            Action::Verified { aci, identity_key, verified } => {
                let conversation = ConversationId::direct(&aci);
                let current = self.identities(&conversation).await.ok().map(|(_, _, _, theirs)| theirs.serialize());
                // Only for the key we have; an older key's verification is moot.
                if current.as_deref() == Some(identity_key.as_slice()) {
                    let value = if verified { hex::encode(&identity_key) } else { String::new() };
                    let _ = self.db.set_setting(&format!("{SETTING_VERIFIED}{aci}"), &value).await;
                    self.emit_safety_number(conversation).await;
                }
            }
            Action::Story { story, pointer } => self.apply_story(manager, story, pointer).await,
            Action::StoryReply { group, story_author, story_sent_at, reply } => {
                self.apply_story_reply(group, story_author, story_sent_at, reply).await
            }
            Action::StoryViewed { viewer, sent_at, at } => self.apply_story_viewed(viewer, sent_at, at).await,
            Action::StoryRecipientUpdate { sent_at, recipients } => self.apply_story_recipient_update(sent_at, recipients).await,
            Action::ViewedSync(viewed) => self.apply_viewed_sync(viewed).await,
            Action::ViewOnceOpen { .. } => {
                self.modify(manager, action).await;
            }
            Action::ReadSync(entries) => {
                let mut changed = false;
                let me = self.self_aci();
                for entry in entries {
                    // One naming our own poll: its votes were read there
                    // (Desktop's `maybeItIsAPollVoteReadSync`).
                    if entry.0 == me {
                        match self.db.mark_poll_votes_read(entry.1).await {
                            Ok(Some(_)) => changed = true,
                            Ok(None) => {}
                            Err(error) => self.error("failed to apply read sync", error),
                        }
                        continue;
                    }
                    changed |= self.modify(manager, Action::ReadSync(vec![entry])).await == early::Outcome::Applied;
                }
                if changed {
                    self.emit(Event::ConversationsChanged);
                }
            }
            Action::DeleteForMe { messages, conversations } => {
                for message in messages {
                    self.modify(manager, Action::DeleteForMe { messages: vec![message], conversations: Vec::new() }).await;
                }
                for chat in conversations {
                    if let Some(conversation) = self.resolve_thread(manager, &chat.thread).await {
                        self.delete_chat_from_sync(&conversation, &chat).await;
                    }
                }
            }
            // Before the first sync (which runs once the queue drains) there
            // is nothing to refresh.
            Action::StorageChanged if *self.storage_synced.borrow() => self.spawn_storage_sync(manager),
            Action::StorageChanged => {}
            Action::LocalProfileChanged => self.local_profile_changed(manager),
            Action::AccountSettings(settings) => self.save_account_settings(settings).await,
            Action::ExpireTimer { conversation, seconds, version, by, at } => {
                self.apply_timer(&conversation, seconds, version, true, by, at).await;
            }
            Action::Pin { conversation, by, target_author, target_sent_at, duration, at } => {
                let Some(target) = self.find(&conversation, target_author.as_deref(), target_sent_at).await else { return };
                self.pin_locally(&conversation, target, by, target_author, target_sent_at, duration, at).await;
            }
            Action::Unpin { conversation, target_author, target_sent_at } => {
                let Some(target) = self.find(&conversation, target_author.as_deref(), target_sent_at).await else { return };
                if let Ok(true) = self.db.unpin_message(&conversation, target).await {
                    self.emit(Event::PinsChanged(conversation));
                }
            }
            Action::PollVote { .. } | Action::PollEnd { .. } => {
                self.modify(manager, action).await;
            }
            Action::MessageRequestResponse { thread, response, at } => {
                self.apply_message_request_sync(manager, thread, response, at).await;
            }
        }
    }

    /// A timer someone else set, as Desktop's `updateExpirationTimer`: in 1:1
    /// chats a message only changes the timer if its version is at least
    /// ours; an explicit update always sets it. Groups only change through
    /// explicit updates (their timer is part of the group state).
    async fn apply_timer(
        &self,
        conversation: &ConversationId,
        seconds: u32,
        version: Option<u32>,
        explicit: bool,
        by: Option<String>,
        at: i64,
    ) {
        let current = self.db.conversation(conversation).await.ok().flatten();
        let current = current.map_or((0, 1), |c| (c.expire_timer, c.expire_timer_version));
        let group = conversation.as_group().is_some();
        let Some((new_seconds, version)) = mapping::timer_update(current, seconds, version, explicit, group) else {
            return;
        };
        if let Err(error) = self.db.set_expire_timer(conversation, new_seconds, version).await {
            return self.error("failed to update timer", error);
        }
        if new_seconds != current.0 {
            self.notice(conversation, at, signal_store::Notice::Timer { by, seconds: new_seconds }).await;
        }
        self.emit(Event::ConversationsChanged);
    }

    /// Adds a timeline notice and shows it.
    async fn notice(&self, conversation: &ConversationId, at: i64, notice: signal_store::Notice) {
        match self.db.insert_notice(conversation, at, &notice).await {
            Ok(Some(id)) => self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id }),
            Ok(None) => {}
            Err(error) => self.error("failed to add notice", error),
        }
    }

    /// Records a pin (ours or someone else's) and its "pinned a message" notice.
    #[allow(clippy::too_many_arguments)]
    async fn pin_locally(
        &self,
        conversation: &ConversationId,
        target: i64,
        by: Option<String>,
        target_author: Option<String>,
        target_sent_at: i64,
        duration: Option<u32>,
        at: i64,
    ) {
        let expires = duration.map(|s| at + i64::from(s) * 1000);
        match self.db.pin_message(conversation, target, at, expires, crate::remote_config().pinned_message_limit()).await {
            Ok(_) => {
                let notice = signal_store::Notice::Pinned { by, author: target_author, sent_at: target_sent_at };
                self.notice(conversation, at, notice).await;
                self.emit(Event::PinsChanged(conversation.clone()));
            }
            Err(error) => self.error("failed to pin message", error),
        }
    }

    /// Pins or unpins a message for everyone in the chat.
    async fn set_pin(self: Rc<Self>, message_id: i64, pin: bool, duration: Option<u32>) {
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        let conversation = message.conversation_id.clone();
        let author = message.author.clone().unwrap_or_else(|| self.self_aci());
        let Some(author_binary) = Aci::parse_from_service_id_string(&author).map(|a| a.service_id_binary()) else {
            return;
        };
        let timestamp = self.next_timestamp();
        let mut data = DataMessage {
            timestamp: Some(timestamp as u64),
            group_v2: self.group_context(&conversation).await,
            ..Default::default()
        };
        if pin {
            use data_message::pin_message::PinDuration;
            self.pin_locally(&conversation, message_id, None, message.author.clone(), message.sent_at, duration, timestamp)
                .await;
            data.pin_message = Some(data_message::PinMessage {
                target_author_aci_binary: Some(author_binary),
                target_sent_timestamp: Some(message.sent_at as u64),
                pin_duration: Some(match duration {
                    Some(seconds) => PinDuration::PinDurationSeconds(seconds),
                    None => PinDuration::PinDurationForever(true),
                }),
            });
        } else {
            if let Ok(true) = self.db.unpin_message(&conversation, message_id).await {
                self.emit(Event::PinsChanged(conversation.clone()));
            }
            data.unpin_message = Some(data_message::UnpinMessage {
                target_author_aci_binary: Some(author_binary),
                target_sent_timestamp: Some(message.sent_at as u64),
            });
        }
        self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp: timestamp as u64, message: None });
    }

    /// Changes a chat's disappearing-message timer for everyone in it.
    async fn set_expire_timer(self: Rc<Self>, conversation: ConversationId, seconds: u32) {
        if conversation.as_group().is_some() {
            return self.set_group_timer(conversation, seconds).await;
        }
        self.set_direct_timer(conversation, seconds).await;
    }

    /// A 1:1 chat's timer: here, a notice, and a timer update to them.
    async fn set_direct_timer(&self, conversation: ConversationId, seconds: u32) {
        let local = self.db.conversation(&conversation).await.ok().flatten().map_or(1, |c| c.expire_timer_version);
        let version = local.saturating_add(1);
        if let Err(error) = self.db.set_expire_timer(&conversation, seconds, version).await {
            return self.error("failed to update timer", error);
        }
        let timestamp = self.next_timestamp() as u64;
        self.notice(&conversation, timestamp as i64, signal_store::Notice::Timer { by: None, seconds }).await;
        self.emit(Event::ConversationsChanged);
        let data = DataMessage {
            flags: Some(data_message::Flags::ExpirationTimerUpdate as u32),
            expire_timer: Some(seconds),
            expire_timer_version: Some(version),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp, message: None });
    }

    /// Group timers change through a group update (Signal's group server).
    async fn set_group_timer(self: Rc<Self>, conversation: ConversationId, seconds: u32) {
        let result = self
            .change_group(&conversation, move |ops, actions| {
                let timer = presage::libsignal_service::groups_v2::Timer { duration: seconds };
                actions.modify_disappearing_message_timer =
                    Some(presage::proto::group_change::actions::ModifyDisappearingMessageTimerAction {
                        timer: ops.encrypt_disappearing_messages_timer(Some(&timer), &mut rand::rng()),
                    });
                Ok(())
            })
            .await;
        if let Err(error) = result {
            self.error("couldn't change the group's timer", format!("{error:#}"));
        }
    }

    async fn update_group(self: Rc<Self>, conversation: ConversationId, title: Option<String>, description: Option<String>) {
        let result = self
            .change_group(&conversation, move |ops, actions| {
                use presage::proto::group_change::actions::{ModifyDescriptionAction, ModifyTitleAction};
                if let Some(title) = title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                    actions.modify_title = Some(ModifyTitleAction { title: ops.encrypt_title(title, &mut rand::rng()) });
                }
                if let Some(description) = &description {
                    let description = Some(description.trim()).filter(|d| !d.is_empty());
                    actions.modify_description =
                        Some(ModifyDescriptionAction { description: ops.encrypt_description(description, &mut rand::rng()) });
                }
                Ok(())
            })
            .await;
        if let Err(error) = result {
            self.error("couldn't update the group", format!("{error:#}"));
        }
    }

    /// Desktop's "New group": a new master key, the group created on the
    /// group server with us as admin, then a message so members see it.
    async fn create_group(self: Rc<Self>, title: String, members: Vec<String>, avatar: Option<Vec<u8>>, expire_timer: u32) {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return self.emit(Event::Error("A group needs a name".into()));
        }
        let Some(manager) = self.manager() else { return };
        let Some(me) = Aci::parse_from_service_id_string(&self.self_aci()) else { return };
        let mine = self.our_profile_key(&manager).await;
        let mut candidates = Vec::new();
        for sid in &members {
            let Some(aci) = Aci::parse_from_service_id_string(sid) else { continue };
            let key = self.store.profile_key(&ServiceId::from(aci)).await.ok().flatten();
            candidates.push((aci, key));
        }
        // Desktop's `createGroupV2` refuses more than the hard limit,
        // counting us (the UI stops picking people before that).
        let limit = crate::remote_config().group_size_hard_limit();
        if candidates.len() + 1 > limit {
            return self.emit(Event::Error(format!("Signal groups can have a maximum of {limit} members.")));
        }
        let key: [u8; 32] = rand::Rng::random(&mut rand::rng());
        let created = async {
            let session = crate::groups::GroupSession::new(&manager, key).await?;
            // Desktop's avatar picker: the picked image made a group photo.
            let photo = match avatar {
                Some(image) => Some(tokio::task::spawn_blocking(move || crate::media::group_avatar(&image)).await??),
                None => None,
            };
            session.create(&title, (me, mine), &candidates, photo.as_deref(), expire_timer).await?;
            let group = session.fetch().await?;
            self.store.save_group(key, group).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = created {
            return self.error("couldn't create the group", format!("{error:#}"));
        }
        let conversation = ConversationId::group(&key);
        self.ensure_title(&conversation).await;
        if let Some(manager) = self.manager() {
            self.refresh_group_avatar(&manager, key, false).await;
        }
        // We made it: never a message request (Desktop sets `addedBy` to us).
        if let Err(error) = self.db.set_message_request(&conversation, signal_store::MessageRequestResponse::Accept).await {
            warn!(%error, "failed to mark our new group accepted");
        }
        self.emit(Event::ConversationsChanged);
        self.emit(Event::GroupCreated(conversation.clone()));
        // Members' apps learn of the group from its first message.
        let timestamp = self.next_timestamp() as u64;
        let data = DataMessage {
            group_v2: Some(GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(0), group_change: None }),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp, message: None });
    }

    /// Desktop's "Add members" (`addMembersToGroup`): people we have a
    /// profile key for join, the rest are invited. Past the group size hard
    /// limit, counting members and invitations, nothing is changed.
    async fn add_group_members(self: Rc<Self>, conversation: ConversationId, members: Vec<String>) {
        let result = async {
            let manager = self.manager().ok_or_else(|| anyhow::anyhow!("not connected"))?;
            let key: [u8; 32] =
                conversation.as_group().and_then(|k| k.try_into().ok()).ok_or_else(|| anyhow::anyhow!("not a group"))?;
            let me = Aci::parse_from_service_id_string(&self.self_aci()).ok_or_else(|| anyhow::anyhow!("no account"))?;
            let _lock = self.group_lock(&conversation).await;
            let session = crate::groups::GroupSession::new(&manager, key).await?;
            let current = session.fetch().await?;
            if current.terminated {
                anyhow::bail!("the group has ended");
            }
            let mut people = Vec::new();
            for sid in &members {
                let Some(aci) = Aci::parse_from_service_id_string(sid) else { continue };
                let already = current.members.iter().any(|m| m.aci == aci)
                    || current.members_pending_profile_key.iter().any(|m| m.address == ServiceId::from(aci));
                if !already {
                    people.push((aci, self.store.profile_key(&ServiceId::from(aci)).await.ok().flatten()));
                }
            }
            if people.is_empty() {
                return Ok(());
            }
            let limit = crate::remote_config().group_size_hard_limit();
            if current.members.len() + current.members_pending_profile_key.len() + people.len() > limit {
                anyhow::bail!("Signal groups can have a maximum of {limit} members.");
            }
            let mut adds = presage::proto::group_change::Actions::default();
            session.add_members_actions(&current, me, &people, &mut adds).await?;
            self.change_group_in(&session, &conversation, key, &current, move |_, actions| {
                actions.add_members = adds.add_members;
                actions.add_members_pending_profile_key = adds.add_members_pending_profile_key;
                actions.delete_members_banned = adds.delete_members_banned;
                Ok(())
            })
            .await
        }
        .await;
        if let Err(error) = result {
            self.error("couldn't add members", format!("{error:#}"));
        }
    }

    /// Desktop's "End group" (`terminateGroup`, `buildTerminateChange`): an
    /// admin ends the group for everyone, for good. Behind the remote
    /// config's `groupTerminate.send`.
    async fn end_group(self: Rc<Self>, conversation: ConversationId) {
        if !crate::remote_config().group_terminate_send() {
            return self.error("couldn't end the group", "not enabled for this account");
        }
        let stored = self.db.conversation(&conversation).await.ok().flatten();
        if stored.as_ref().is_some_and(|c| c.ended || c.left) {
            return;
        }
        if !self.is_group_admin(&conversation, &self.self_aci()).await {
            return self.error("couldn't end the group", "only admins can end a group");
        }
        let result = self
            .change_group(&conversation, |_, actions| {
                actions.terminate_group = Some(presage::proto::group_change::actions::TerminateGroupAction {});
                Ok(())
            })
            .await;
        if let Err(error) = result {
            warn!(error = %format!("{error:#}"), "couldn't end the group");
            // Desktop's `TerminateGroupFailedModal`.
            self.emit(Event::Error("Couldn't end the group. Check your connection and try again.".into()));
        }
    }

    /// Our label in a group (Signal's member labels), encrypted like other
    /// group strings. Desktop allows 24 characters (96 bytes) and one emoji.
    async fn set_member_label(self: Rc<Self>, conversation: ConversationId, emoji: Option<String>, text: String) {
        let text = text.trim().to_owned();
        let emoji = emoji.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty() && !text.is_empty());
        if text.chars().count() > 24 || text.len() > 96 || emoji.as_ref().is_some_and(|e| e.len() > 48) {
            return self.error("can't set label", "labels are at most 24 characters");
        }
        let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return };
        let Some(aci) = Aci::parse_from_service_id_string(&self.self_aci()) else { return };
        let secret = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key));
        let encrypt = |plain: &str| {
            if plain.is_empty() {
                return Vec::new();
            }
            let randomness: [u8; 32] = rand::Rng::random(&mut rand::rng());
            secret.encrypt_blob_with_padding(randomness, plain.as_bytes(), 0)
        };
        let (label_string, label_emoji) = (encrypt(&text), encrypt(emoji.as_deref().unwrap_or("")));
        let result = self
            .change_group(&conversation, move |ops, actions| {
                // Our encrypted ACI, as a member change names us.
                let user_id = ops.build_remove_member_action(aci).map_err(|e| anyhow::anyhow!("{e:?}"))?.deleted_user_id;
                actions.modify_member_labels.push(presage::proto::group_change::actions::ModifyMemberLabelAction {
                    user_id,
                    label_emoji,
                    label_string,
                });
                Ok(())
            })
            .await;
        if let Err(error) = result {
            self.error("couldn't set your label", format!("{error:#}"));
        }
    }

    /// Leaves a group. Like Desktop, the last admin must name another first.
    /// An invitation we haven't taken is declined.
    async fn leave_group(self: Rc<Self>, conversation: ConversationId) {
        // Desktop's `leaveGroupV2`: an invitation is declined.
        if matches!(self.stored_group_state(&conversation).await.membership, signal_store::Membership::Invited { .. }) {
            self.decline_invite(&conversation).await;
            return;
        }
        let me = self.self_aci();
        let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return };
        if let Ok(Some(group)) = self.store.group(key).await {
            use presage::libsignal_service::groups_v2::Role;
            let admin = |m: &presage::model::groups::Member| m.role == Role::Administrator;
            let me_admin = group.members.iter().any(|m| m.aci.service_id_string() == me && admin(m));
            let other_admins = group.members.iter().any(|m| m.aci.service_id_string() != me && admin(m));
            if me_admin && !other_admins && group.members.len() > 1 {
                return self.emit(Event::Error("Make another member an admin before you leave this group".into()));
            }
        }
        let Some(aci) = Aci::parse_from_service_id_string(&me) else { return };
        let result = self
            .change_group(&conversation, move |ops, actions| {
                actions.delete_members.push(ops.build_remove_member_action(aci).map_err(|e| anyhow::anyhow!("{e:?}"))?);
                Ok(())
            })
            .await;
        if let Err(error) = result {
            self.error("couldn't leave the group", format!("{error:#}"));
        }
    }

    /// Desktop's `modifyGroupV2`: builds a change for the group's next
    /// revision, has the group server apply it, applies it here, then sends
    /// it to the members (so their apps update without asking the server).
    async fn change_group(
        self: &Rc<Self>,
        conversation: &ConversationId,
        build: impl FnOnce(
            &presage::libsignal_service::groups_v2::GroupOperations,
            &mut presage::proto::group_change::Actions,
        ) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let manager = self.manager().ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let key: [u8; 32] =
            conversation.as_group().and_then(|k| k.try_into().ok()).ok_or_else(|| anyhow::anyhow!("not a group"))?;
        let _lock = self.group_lock(conversation).await;
        let session = crate::groups::GroupSession::new(&manager, key).await?;
        let current = session.fetch().await?;
        self.change_group_in(&session, conversation, key, &current, build).await
    }

    /// [`Self::change_group`] with the session and the group as it is now
    /// already in hand.
    async fn change_group_in(
        self: &Rc<Self>,
        session: &crate::groups::GroupSession,
        conversation: &ConversationId,
        key: [u8; 32],
        current: &presage::libsignal_service::groups_v2::Group,
        build: impl FnOnce(
            &presage::libsignal_service::groups_v2::GroupOperations,
            &mut presage::proto::group_change::Actions,
        ) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut actions = presage::proto::group_change::Actions { version: current.version + 1, ..Default::default() };
        build(&session.operations, &mut actions)?;
        self.submit_group_change(session, conversation, key, current, actions, Vec::new()).await
    }

    /// Has the group server apply `actions`, applies the change here, then
    /// sends it to the members, the people invited, and `extra` (Desktop's
    /// `modifyGroupV2`, with `getGroupV2Info({ includePendingMembers: true,
    /// extraConversationsForSend })`).
    pub(super) async fn submit_group_change(
        self: &Rc<Self>,
        session: &crate::groups::GroupSession,
        conversation: &ConversationId,
        key: [u8; 32],
        current: &presage::libsignal_service::groups_v2::Group,
        actions: presage::proto::group_change::Actions,
        extra: Vec<String>,
    ) -> anyhow::Result<()> {
        let signed = session.modify(actions).await?;
        let timestamp = self.next_timestamp();
        // The members to tell are the ones before the change (when leaving,
        // the group is no longer ours to read).
        let context = GroupContextV2 {
            master_key: Some(key.to_vec()),
            revision: Some(current.version + 1),
            group_change: Some(signed.clone()),
        };
        let expire_timer = self.db.conversation(conversation).await.ok().flatten().map(|c| c.expire_timer);
        self.apply_group_change(key, &signed, timestamp).await;
        let left = self.db.conversation(conversation).await.ok().flatten().is_some_and(|c| c.left);
        let after = if left { None } else { session.fetch().await.ok() };
        // Those invited, before or after the change, hear of it too.
        let me = self.self_aci();
        let group = after.as_ref().unwrap_or(current);
        let members: HashSet<String> = group.members.iter().map(|m| m.aci.service_id_string()).collect();
        let mut others: Vec<String> = current
            .members_pending_profile_key
            .iter()
            .chain(after.iter().flat_map(|g| &g.members_pending_profile_key))
            .filter_map(|p| match p.address {
                ServiceId::Aci(aci) => Some(aci.service_id_string()),
                ServiceId::Pni(_) => None,
            })
            .chain(extra.into_iter().filter(|sid| Aci::parse_from_service_id_string(sid).is_some()))
            .filter(|sid| *sid != me && !members.contains(sid))
            .collect();
        others.sort();
        others.dedup();
        if let Some(group) = after {
            self.save_full_group(key, group).await;
        }
        self.titled.borrow_mut().remove(conversation);
        self.ensure_title(conversation).await;
        self.emit(Event::ConversationsChanged);
        let data = DataMessage {
            group_v2: Some(context),
            timestamp: Some(timestamp as u64),
            expire_timer: expire_timer.filter(|t| *t > 0),
            ..Default::default()
        };
        for recipient in others {
            self.enqueue(Outbound::GroupUpdateTo { recipient, body: data.clone().into(), timestamp: timestamp as u64 });
        }
        self.enqueue(Outbound::Send { conversation: conversation.clone(), body: data.into(), timestamp: timestamp as u64, message: None });
        Ok(())
    }

    /// A group change (ours or someone else's): notices for what changed,
    /// the new timer, and whether we're still in the group.
    async fn apply_group_change(self: &Rc<Self>, key: [u8; 32], change: &[u8], at: i64) {
        use presage::libsignal_service::groups_v2::GroupOperations;
        use prost::Message as _;
        let operations = GroupOperations::new(GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key)));
        let Ok(change) = presage::proto::GroupChange::decode(change) else { return };
        let changes = match operations.decrypt_group_change(change) {
            Ok(changes) => changes,
            Err(error) => return warn!(?error, "couldn't decrypt a group change"),
        };
        let conversation = ConversationId::group(&key);
        let pni = self.manager().map(|m| m.registration_data().service_ids.pni().service_id_string());
        let summary = {
            let _lock = self.group_state_lock(&conversation).await;
            let before = self.stored_group_state(&conversation).await;
            let summary = mapping::summarize_group_change(&changes, &self.self_aci(), pni.as_deref(), &before);
            let mut state = before.clone();
            mapping::apply_to_group_state(&summary, &mut state);
            if state != before {
                let update = ConversationUpdate { group_state: Some(state), ..Default::default() };
                if let Err(error) = self.db.upsert_conversation(&conversation, update).await {
                    warn!(%error, "failed to save the group's state");
                }
            }
            summary
        };
        match summary.membership {
            Some(mapping::MembershipChange::Joined | mapping::MembershipChange::Invited { .. }) => {
                let _ = self.db.set_left(&conversation, false).await;
            }
            // Turned down, or our invitation's gone: no longer in it at all.
            Some(mapping::MembershipChange::Out) => {
                let _ = self.db.set_left(&conversation, true).await;
            }
            Some(mapping::MembershipChange::Requesting) | None => {}
        }
        if let Some(seconds) = summary.timer {
            let version = self.db.conversation(&conversation).await.ok().flatten().map_or(1, |c| c.expire_timer_version);
            let _ = self.db.set_expire_timer(&conversation, seconds, version).await;
        }
        if summary.ended {
            // Desktop's `terminated`: for good, so nothing is editable.
            let update = ConversationUpdate {
                ended: Some(true),
                can_edit_info: Some(false),
                can_add_members: Some(false),
                can_label: Some(false),
                ..Default::default()
            };
            if let Err(error) = self.db.upsert_conversation(&conversation, update).await {
                warn!(%error, "failed to record that a group ended");
            }
        }
        if summary.added_by.is_some() {
            let update = ConversationUpdate { added_by: summary.added_by.clone(), ..Default::default() };
            if let Err(error) = self.db.upsert_conversation(&conversation, update).await {
                warn!(%error, "failed to record who added us");
            }
        }
        if summary.left {
            let _ = self.db.set_left(&conversation, true).await;
            let _ = self.db.set_can_edit_info(&conversation, false).await;
        } else if summary.notices.iter().any(|n| {
            matches!(n, signal_store::Notice::Group { change: signal_store::GroupNotice::Added { member: None }, .. })
        }) {
            // Added back.
            let _ = self.db.set_left(&conversation, false).await;
        }
        let new_photo = summary
            .notices
            .iter()
            .any(|n| matches!(n, signal_store::Notice::Group { change: signal_store::GroupNotice::Avatar, .. }));
        if new_photo && let Some(manager) = self.manager() {
            let this = self.clone();
            tokio::task::spawn_local(async move {
                if this.refresh_group_avatar(&manager, key, true).await {
                    this.emit(Event::ConversationsChanged);
                }
            });
        }
        // One notice per change, a millisecond apart (notices dedupe by time).
        for (ix, notice) in summary.notices.into_iter().enumerate() {
            self.notice(&conversation, at + ix as i64, notice).await;
        }
        self.titled.borrow_mut().remove(&conversation);
        self.ensure_title(&conversation).await;
        self.emit(Event::ConversationsChanged);
    }

    /// Removes messages whose disappearing countdown has run out, sleeping
    /// until the next one is due (or a new countdown starts).
    async fn expiry_loop(self: Rc<Self>) {
        loop {
            let now = now_millis();
            match self.db.expired_messages(now).await {
                Ok(expired) => {
                    for (id, conversation) in expired {
                        self.remove_message(&conversation, id).await;
                    }
                }
                Err(error) => warn!(%error, "failed to read expired messages"),
            }
            let next = self.db.next_expiry().await.ok().flatten();
            // Wake at least hourly, in case the clock jumped.
            let wait = next.map_or(3_600_000, |at| (at - now_millis()).clamp(0, 3_600_000));
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(wait as u64)) => {}
                _ = self.expiry.notified() => {}
            }
        }
    }

    async fn find(&self, conversation: &ConversationId, author: Option<&str>, sent_at: i64) -> Option<i64> {
        match self.db.find_message(conversation, author, sent_at).await {
            Ok(m) => m.map(|m| m.id),
            Err(error) => {
                self.error("failed to look up message", error);
                None
            }
        }
    }

    /// "@Name" for each mention placeholder, and where each mention and
    /// style lands.
    async fn render_body(
        self: &Rc<Self>,
        body: &str,
        mentions: &[String],
        styles: &[mapping::WireStyle],
    ) -> (String, Vec<signal_store::MentionSpan>, Vec<signal_store::StyleSpan>) {
        let mut names = Vec::with_capacity(mentions.len());
        for aci in mentions {
            names.push(if *aci == self.self_aci() {
                "You".to_owned()
            } else {
                self.name_of(aci).await.unwrap_or_else(|| short_id(aci))
            });
        }
        mapping::render_body(body, mentions, &names, styles)
    }


    // ------------------------------------------------------------------ names

    /// Best-known display name for a service id: synced contact name, then
    /// cached profile name. Kicks off a profile fetch when neither is known.
    async fn name_of(self: &Rc<Self>, service_id: &str) -> Option<String> {
        if let Some(cached) = self.names.borrow().get(service_id) {
            return cached.clone();
        }
        let sid = ServiceId::parse_from_service_id_string(service_id)?;
        let mut name = self.db.contact_name(service_id).await.ok().flatten();
        if name.is_none()
            && let Ok(Some(contact)) = self.store.contact_by_id(&sid).await
            && !contact.name.trim().is_empty()
        {
            name = Some(contact.name.trim().to_owned());
        }
        if name.is_none()
            && let Some(key) = self.profile_key_of(&sid).await
        {
            if let Ok(Some(profile)) = self.store.profile(sid.raw_uuid(), key).await {
                name = profile.name.map(|n| n.to_string().trim().to_owned()).filter(|n| !n.is_empty());
            }
            if name.is_none() {
                self.fetch_profile(service_id.to_owned(), key);
            }
        }
        self.names.borrow_mut().insert(service_id.to_owned(), name.clone());
        name
    }

    /// Profile key from the store, else from a group we share with them (group
    /// state carries every member's key; presage doesn't copy it over).
    async fn profile_key_of(&self, sid: &ServiceId) -> Option<ProfileKey> {
        if let Ok(Some(key)) = self.store.profile_key(sid).await {
            return Some(key);
        }
        let ServiceId::Aci(aci) = sid else { return None };
        let key = self
            .store
            .groups()
            .await
            .ok()?
            .flatten()
            .find_map(|(_, group)| group.members.into_iter().find(|m| m.aci == *aci).map(|m| m.profile_key))?;
        let mut store = self.store.clone();
        if let Err(error) = store.upsert_profile_key(&(*aci).into(), key).await {
            debug!(%error, "failed to save group member profile key");
        }
        Some(key)
    }

    /// Messages keep the author name known when they arrived. Once names are
    /// learned (storage sync, contact sync, profile fetch), rewrite the ones
    /// that were missing or have changed.
    async fn backfill_author_names(self: &Rc<Self>) {
        let authors = match self.db.message_authors().await {
            Ok(authors) => authors,
            Err(error) => return self.error("failed to list message authors", error),
        };
        let self_aci = self.self_aci();
        let mut names = Vec::new();
        for author in authors.into_iter().filter(|a| *a != self_aci) {
            if let Some(name) = self.name_of(&author).await {
                names.push((author, name));
            }
        }
        self.rename_authors(names).await;
    }

    async fn rename_authors(self: &Rc<Self>, names: Vec<(String, String)>) {
        match self.db.rename_authors(&names).await {
            Ok(0) => {}
            Ok(_) => {
                self.emit(Event::AuthorNamesChanged(names));
                self.emit(Event::ConversationsChanged);
            }
            Err(error) => self.error("failed to update author names", error),
        }
    }

    fn fetch_profile(
        self: &Rc<Self>,
        service_id: String,
        key: presage::libsignal_service::prelude::ProfileKey,
    ) {
        if !self.profile_fetches.borrow_mut().insert(service_id.clone()) {
            return;
        }
        let Some(mut manager) = self.manager() else { return };
        let Some(aci) = Aci::parse_from_service_id_string(&service_id) else { return };
        let this = self.clone();
        tokio::task::spawn_local(async move {
            match manager.retrieve_profile_by_uuid(aci, key).await {
                Ok(profile) => {
                    let name = profile.name.map(|n| n.to_string().trim().to_owned()).filter(|n| !n.is_empty());
                    if name.is_some() {
                        this.names.borrow_mut().insert(service_id.clone(), name.clone());
                        let conversation = ConversationId::direct(&service_id);
                        this.titled.borrow_mut().remove(&conversation);
                        this.ensure_title(&conversation).await;
                        this.emit(Event::ConversationsChanged);
                        if let Some(name) = name {
                            this.rename_authors(vec![(service_id, name)]).await;
                        }
                    }
                }
                Err(error) => debug!(%error, "profile fetch failed"),
            }
        });
    }

    fn spawn_storage_sync(self: &Rc<Self>, manager: &SignalManager) {
        let this = self.clone();
        let manager = manager.clone();
        tokio::task::spawn_local(async move {
            this.sync_storage(&manager).await;
            this.backfill_author_names().await;
            this.sync_avatars(manager).await;
        });
    }

    /// Pins or unpins for the whole account. The list updates right away; if
    /// the Storage Service write fails, re-reading it puts things back.
    async fn set_pinned(self: Rc<Self>, conversation: ConversationId, pinned: bool) {
        let Some(manager) = self.manager() else { return };
        let mut pins = self.db.pinned_ids().await.unwrap_or_default();
        pins.retain(|id| *id != conversation);
        if pinned {
            let limit = crate::remote_config().pinned_chat_limit();
            if pins.len() >= limit {
                return self.emit(Event::Error(format!("You can pin up to {limit} chats")));
            }
            pins.push(conversation.clone());
        }
        if let Ok(true) = self.db.replace_pinned(&pins).await {
            self.emit(Event::ConversationsChanged);
        }

        let edit = if pinned { storage::PinEdit::Pin(conversation) } else { storage::PinEdit::Unpin(conversation) };
        match storage::write_pin(&manager, &edit).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't update pinned chats", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// Tells our other devices to pick up a Storage Service change, as
    /// Signal Desktop does.
    fn storage_written(&self) {
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::FetchLatest(sync_message::FetchLatest {
                    r#type: Some(sync_message::fetch_latest::Type::StorageManifest as i32),
                })),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Changes a chat here right away, then for the whole account. If the
    /// Storage Service write fails, re-reading it puts things back.
    async fn set_chat(self: Rc<Self>, conversation: ConversationId, change: crate::ChatChange) {
        use crate::ChatChange;
        let Some(manager) = self.manager() else { return };
        if self.is_note_to_self(&conversation) && matches!(change, ChatChange::Blocked(true)) {
            return;
        }
        let local = match change {
            ChatChange::Archived(on) => self.db.set_archived(&conversation, on).await,
            ChatChange::MutedUntil(until) => self.db.set_muted_until(&conversation, until).await,
            ChatChange::MarkedUnread(on) => self.db.set_marked_unread(&conversation, on).await,
            ChatChange::Blocked(on) => self.db.set_blocked(&conversation, on).await,
            ChatChange::HideStory(on) => self.set_story_hidden(&conversation, on).await,
        };
        if let Err(error) = local {
            return self.error("failed to update chat", error);
        }
        self.emit(Event::ConversationsChanged);
        let edit = match change {
            ChatChange::Archived(on) => storage::ChatEdit::Archived(on),
            ChatChange::MutedUntil(until) => storage::ChatEdit::MutedUntil(until.max(0) as u64),
            ChatChange::MarkedUnread(on) => storage::ChatEdit::MarkedUnread(on),
            ChatChange::Blocked(on) => storage::ChatEdit::Blocked(on),
            ChatChange::HideStory(on) => storage::ChatEdit::HideStory(on),
        };
        match storage::write_chat(&manager, &conversation, edit).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't update the chat on your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
        // Desktop's "Block and Leave" for groups.
        let in_group = self.db.conversation(&conversation).await.ok().flatten().is_some_and(|c| !c.left);
        if matches!(change, ChatChange::Blocked(true)) && conversation.as_group().is_some() && in_group {
            self.leave_group(conversation).await;
        }
    }

    /// Desktop's `updateNicknameAndNote`: the nickname and note change here
    /// right away (the chat's title, its messages' author names, search),
    /// then in Storage Service. If that write fails, re-reading it puts
    /// things back.
    async fn set_nickname(self: Rc<Self>, conversation: ConversationId, nickname: signal_store::ContactNickname) {
        let Some(manager) = self.manager() else { return };
        // Desktop's `canHaveNicknameAndNote`: people, not groups, the
        // Signal chat or ourselves.
        let Some(sid) = conversation.as_direct().map(str::to_owned) else { return };
        if conversation.is_signal() || self.is_note_to_self(&conversation) {
            return;
        }
        let nickname = signal_store::limit_nickname(&nickname);
        if let Err(error) = self.db.set_contact_nickname(&sid, &nickname).await {
            return self.error("failed to save the nickname", error);
        }
        self.names.borrow_mut().remove(&sid);
        self.titled.borrow_mut().remove(&conversation);
        let name = self.name_of(&sid).await;
        match &name {
            Some(name) => {
                if let Err(error) = self.db.retitle_directory_entry(&conversation, name).await {
                    warn!(%error, "failed to rename directory entry");
                }
                self.ensure_title(&conversation).await;
            }
            // No name left (no profile name known yet): the number, rather
            // than keep showing the nickname.
            None => {
                let phone = self.db.conversation(&conversation).await.ok().flatten().and_then(|c| c.phone);
                let update = ConversationUpdate { title: Some(phone.unwrap_or_else(|| short_id(&sid))), ..Default::default() };
                if let Err(error) = self.db.upsert_conversation(&conversation, update).await {
                    self.error("failed to update conversation", error);
                }
            }
        }
        self.emit(Event::ConversationsChanged);
        if let Some(name) = name {
            self.rename_authors(vec![(sid, name)]).await;
        }
        match storage::write_nickname(&manager, &conversation, &nickname).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't save the nickname on your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// Reads names, profile keys, pins and archived/muted state from the
    /// Storage Service, then re-titles.
    async fn sync_storage(self: &Rc<Self>, manager: &SignalManager) {
        let storage::StorageSnapshot {
            contacts,
            pinned,
            chats,
            settings,
            story_lists,
            hidden_stories,
            stories_disabled,
            story_view_receipts,
            my_story_privacy_set,
            call_links,
            profile_sharing,
            story_send_modes,
            backup_tier,
            identifiers,
            own_profile,
        } = match storage::fetch(manager).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "storage service sync failed");
                return;
            }
        };
        info!(contacts = contacts.len(), pinned = ?pinned.as_ref().map(Vec::len), "read storage service");
        if let Some(pinned) = pinned {
            match self.db.replace_pinned(&pinned).await {
                Ok(true) => self.emit(Event::ConversationsChanged),
                Ok(false) => {}
                Err(error) => self.error("failed to save pinned conversations", error),
            }
        }
        if settings != crate::AccountSettings::default() {
            self.account_record_read.set(true);
        }
        self.save_account_settings(settings).await;
        if let Some(tier) = backup_tier {
            self.save_backup_tier(tier).await;
        }
        {
            let _edit = self.story_lists_edit.lock().await;
            self.save_story_settings(
                Some(story_lists),
                Some(hidden_stories),
                stories_disabled,
                story_view_receipts,
                my_story_privacy_set,
            )
            .await;
        }
        self.apply_storage_call_links(call_links).await;
        if let Err(error) = self.db.apply_profile_sharing(&profile_sharing).await {
            warn!(error = %format!("{error:#}"), "failed to save profile sharing");
        }
        match self.db.set_story_send_modes(&story_send_modes).await {
            Ok(true) => self.emit(Event::StorySettingsChanged),
            Ok(false) => {}
            Err(error) => warn!(error = %format!("{error:#}"), "failed to save groups' story send modes"),
        }
        if let Some(identifiers) = &identifiers {
            self.apply_account_identifiers(identifiers).await;
        }
        if let Some(own_profile) = own_profile {
            self.apply_own_profile_record(manager, own_profile).await;
        }
        match self.db.apply_chat_states(&chats).await {
            Ok(true) => self.emit(Event::ConversationsChanged),
            Ok(false) => {}
            Err(error) => self.error("failed to save archived and muted chats", error),
        }
        let mut store = self.store.clone();
        let mut names = Vec::new();
        let mut nicknames = Vec::new();
        for contact in &contacts {
            let service_id = Aci::from(contact.aci).service_id_string();
            if let Some(key) = contact.profile_key
                && let Err(error) = store.upsert_profile_key(&contact.aci, ProfileKey::create(key)).await
            {
                debug!(%error, "failed to save profile key");
            }
            if !contact.nickname.is_empty() {
                nicknames.push((service_id.clone(), contact.nickname.clone()));
            }
            if let Some(name) = &contact.name {
                names.push((service_id, name.clone()));
            }
        }
        let usernames: Vec<(String, String)> = contacts
            .iter()
            .filter_map(|c| Some((Aci::from(c.aci).service_id_string(), c.username.clone()?)))
            .collect();
        if let Err(error) = self.db.replace_usernames(&usernames).await {
            warn!(%error, "failed to save contacts' usernames");
        }
        if let Err(error) = self.db.replace_contacts(&names, &nicknames).await {
            self.error("failed to save contact names", error);
            return;
        }
        self.names.borrow_mut().clear();
        self.titled.borrow_mut().clear();
        self.refresh_titles().await;
        self.refresh_directory(&contacts).await;
    }

    /// Installed packs from presage's store, each sticker written to
    /// `stickers/<pack>/<id>.<ext>` once so it can be shown from disk.
    async fn load_stickers(&self) -> Vec<crate::StickerPackView> {
        let Ok(packs) = self.store.sticker_packs().await else { return Vec::new() };
        let root = self.config.data_dir.join("stickers");
        let mut views = Vec::new();
        for pack in packs.flatten() {
            let dir = root.join(hex_of(&pack.id));
            if std::fs::create_dir_all(&dir).is_err() {
                continue;
            }
            let stickers = pack
                .manifest
                .stickers
                .iter()
                .filter_map(|sticker| {
                    let bytes = sticker.bytes.as_ref()?;
                    let ext = media::ImageKind::sniff(bytes).map_or("webp", |k| k.extension());
                    let path = dir.join(format!("{}.{ext}", sticker.id));
                    if !path.exists() {
                        std::fs::write(&path, bytes).ok()?;
                    }
                    Some(crate::StickerView { id: sticker.id, emoji: sticker.emoji.clone(), path })
                })
                .collect::<Vec<_>>();
            if !stickers.is_empty() {
                views.push(crate::StickerPackView {
                    id: pack.id.clone(),
                    key: pack.key.clone(),
                    title: pack.manifest.title.clone(),
                    author: pack.manifest.author.clone(),
                    stickers,
                });
            }
        }
        views
    }

    /// Downloads the GIF (through Signal's content proxy), shows it at once,
    /// and queues the upload. It replies to `quote` when given, as Desktop's
    /// GIF goes with the composer's quote (`sendMultiMediaMessage`).
    async fn send_gif(self: Rc<Self>, conversation: ConversationId, gif: crate::giphy::Gif, quote: Option<i64>) {
        let bytes = match crate::giphy::download(&gif.original.url).await {
            Ok(bytes) => bytes,
            Err(error) => return self.error("couldn't get the GIF", format!("{error:#}")),
        };
        let path = self.config.data_dir.join("attachments").join(format!("sent-{}.mp4", now_millis()));
        if let Err(error) = tokio::fs::write(&path, &bytes).await {
            return self.error("failed to save GIF", error);
        }
        let meta = AttachmentMeta {
            content_type: "video/mp4".into(),
            file_name: None,
            size: bytes.len() as u64,
            width: Some(gif.original.width),
            height: Some(gif.original.height),
            local_path: Some(path.to_string_lossy().into_owned()),
            sticker: false,
            gif: true,
            voice: false,
            remote: None,
            thumbnail_path: None,
        };
        let Some((id, data, timestamp)) = self.store_outgoing(&conversation, String::new(), quote, vec![meta], None).await
        else {
            return;
        };
        let size = (gif.original.width, gif.original.height);
        self.enqueue(Outbound::SendGif { conversation, body: data, timestamp, message: id, path, size });
    }

    async fn upload_gif(
        &self,
        manager: &SignalManager,
        path: &std::path::Path,
        (width, height): (u32, u32),
    ) -> anyhow::Result<AttachmentPointer> {
        let bytes = tokio::fs::read(path).await?;
        let spec = AttachmentSpec {
            content_type: "video/mp4".into(),
            length: bytes.len(),
            file_name: None,
            preview: None,
            voice_note: None,
            borderless: None,
            width: Some(width),
            height: Some(height),
            caption: None,
            blur_hash: None,
        };
        let mut pointer = manager
            .upload_attachment(spec, bytes)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("uploading GIF: {e}"))?;
        // What makes Signal clients loop it silently.
        pointer.flags = Some(pointer.flags.unwrap_or(0) | attachment_pointer::Flags::Gif as u32);
        Ok(pointer)
    }

    /// A sticker, replying to `quote` when given (Desktop's
    /// `stickerReply.send`, `ts/state/ducks/composer.preload.ts`).
    async fn send_sticker(
        self: Rc<Self>,
        conversation: ConversationId,
        pack: &crate::StickerPackView,
        sticker: crate::StickerView,
        quote: Option<i64>,
    ) {
        let Ok(bytes) = tokio::fs::read(&sticker.path).await else {
            return self.error("can't send sticker", "sticker image is missing");
        };
        let (kind, width, height) = media::probe(&bytes).unwrap_or((media::ImageKind::Webp, 512, 512));
        let meta = AttachmentMeta {
            content_type: kind.content_type().into(),
            file_name: None,
            size: bytes.len() as u64,
            width: Some(width),
            height: Some(height),
            local_path: Some(sticker.path.to_string_lossy().into_owned()),
            sticker: true,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        };
        let Some((id, mut data, timestamp)) = self.store_outgoing(&conversation, String::new(), quote, vec![meta], None).await
        else {
            return;
        };
        data.sticker = Some(data_message::Sticker {
            pack_id: Some(pack.id.clone()),
            pack_key: Some(pack.key.clone()),
            sticker_id: Some(sticker.id),
            emoji: sticker.emoji,
            data: None,
        });
        self.enqueue(Outbound::SendSticker { conversation, body: data, timestamp, message: id, path: sticker.path });
    }

    /// A username or phone number, looked up on Signal's servers.
    async fn find_recipient(&self, query: &str) -> Result<signal_store::DirectoryEntry, String> {
        let mut manager = self.manager().ok_or("not connected")?;
        // A `signal.me/#eu/…` link: the username it holds (Desktop's
        // `showConversationViaSignalDotMe`, which shows its "didn't make
        // sense" error when the link is gone or can't be read).
        let linked = match crate::routes::parse(query) {
            Some(crate::routes::Route::ContactByEncryptedUsername(encrypted)) => {
                let link = url::Url::parse(&crate::routes::Route::username_link(&encrypted)).map_err(|_| crate::routes::UNKNOWN_LINK)?;
                match manager.lookup_username_link(&link).await {
                    Ok(Some(username)) => Some(username),
                    Ok(None) => return Err(crate::routes::UNKNOWN_LINK.into()),
                    Err(error) => {
                        warn!(error = %error, "couldn't read a username link");
                        return Err(crate::routes::UNKNOWN_LINK.into());
                    }
                }
            }
            Some(crate::routes::Route::ContactByPhoneNumber(number)) => Some(number),
            _ => None,
        };
        let query = linked.as_deref().unwrap_or(query);
        if let Some(username) = crate::recipients::username_in(query) {
            return match manager.lookup_username(&username).await {
                Ok(Some(aci)) => Ok(signal_store::DirectoryEntry {
                    id: ConversationId::direct(&aci.service_id_string()),
                    kind: ConversationKind::Direct,
                    title: username,
                    phone: None,
                }),
                Ok(None) => Err(format!("No one on Signal has the username {username}")),
                Err(error) => Err(format!("Couldn't look up {username}: {error}")),
            };
        }
        let home = manager.registration_data().phone_number.to_string();
        let Some(e164) = crate::recipients::phone_in(query, Some(&home)) else {
            return Err("Enter a phone number or a username like name.12".into());
        };
        match manager.discover_contacts_by_phone_number([e164.as_str()]).await {
            Ok(found) => match found.into_iter().find_map(|(_, sid)| sid) {
                Some(sid) => Ok(signal_store::DirectoryEntry {
                    id: ConversationId::direct(&sid.service_id_string()),
                    kind: ConversationKind::Direct,
                    title: e164.clone(),
                    phone: Some(e164),
                }),
                None => Err(format!("{e164} isn't on Signal")),
            },
            Err(error) => Err(format!("Couldn't look up {e164}: {error}")),
        }
    }

    /// Everyone and every group this account knows, for search and new
    /// chats: Storage Service contacts (minus hidden and blocked ones), the
    /// phone's contact list, and the groups we're in.
    async fn refresh_directory(self: &Rc<Self>, storage_contacts: &[storage::StorageContact]) {
        let me = self.self_aci();
        let mut entries: HashMap<String, signal_store::DirectoryEntry> = HashMap::new();
        let mut add = |sid: String, title: Option<String>, phone: Option<String>| {
            if sid == me {
                return;
            }
            let Some(title) = title.or_else(|| phone.clone()) else { return };
            entries.entry(sid.clone()).or_insert(signal_store::DirectoryEntry {
                id: ConversationId::direct(&sid),
                kind: ConversationKind::Direct,
                title,
                phone,
            });
        };
        // Who's in our address book (Desktop's `isInSystemContacts`): a
        // system name on their contact record, or a name from the phone's
        // contact sync.
        let mut system: Vec<String> =
            storage_contacts.iter().filter(|c| c.system_name).map(|c| Aci::from(c.aci).service_id_string()).collect();
        for contact in storage_contacts.iter().filter(|c| !c.hidden) {
            add(Aci::from(contact.aci).service_id_string(), contact.name.clone(), contact.e164.clone());
        }
        if let Ok(contacts) = self.store.contacts().await {
            for contact in contacts.flatten() {
                let name = Some(contact.name.trim().to_owned()).filter(|n| !n.is_empty());
                if name.is_some() {
                    system.push(contact.uuid.to_string());
                }
                add(contact.uuid.to_string(), name, contact.phone_number.map(|p| p.to_string()));
            }
        }
        match self.db.replace_system_contacts(&system).await {
            Ok(true) => self.emit(Event::ConversationsChanged),
            Ok(false) => {}
            Err(error) => warn!(%error, "failed to save who's in the address book"),
        }
        let mut entries: Vec<_> = entries.into_values().collect();
        if let Ok(groups) = self.store.groups().await {
            for (key, group) in groups.flatten() {
                let title = Some(group.title.trim().to_owned()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Group".into());
                entries.push(signal_store::DirectoryEntry {
                    id: ConversationId::group(&key),
                    kind: ConversationKind::Group,
                    title,
                    phone: None,
                });
            }
        }
        info!(entries = entries.len(), "refreshed directory");
        if let Err(error) = self.db.replace_directory(&entries).await {
            warn!(%error, "failed to save directory");
        }
    }

    /// Brings group and profile photos up to date. They are decrypted into
    /// `avatars/`, like attachments.
    async fn sync_avatars(self: &Rc<Self>, manager: SignalManager) {
        let dir = self.config.data_dir.join("avatars");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            warn!(%error, "failed to create avatars directory");
            return;
        }
        let mut conversations = self.db.conversations().await.unwrap_or_default();
        conversations.extend(self.db.archived_conversations().await.unwrap_or_default());
        let mut changed = false;
        // Groups are checked every time: a group's copy is local, so it's cheap.
        for conversation in &conversations {
            if let Some(key) = conversation.id.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) {
                changed |= self.refresh_group_avatar(&manager, key, false).await;
            }
        }
        let (people_changed, more) = self.sync_person_avatars(&manager, &conversations, &dir).await;
        // Those profiles named badges; fetch their images.
        self.download_badges();
        if changed || people_changed {
            self.emit(Event::ConversationsChanged);
        }
        // The next batch of profile photos, a little later (after linking,
        // most everyone's is missing).
        if more && !self.avatar_batch_pending.replace(true) {
            let this = self.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(AVATAR_BATCH_DELAY).await;
                this.avatar_batch_pending.set(false);
                if let Some(manager) = this.manager() {
                    Box::pin(this.sync_avatars(manager)).await;
                }
            });
        }
    }

    /// Profile photos of everyone shown with one: 1:1 contacts and group
    /// members. Each check is a profile fetch, so, like Desktop's routine
    /// profile refresh, a person is checked at most daily and a sync makes
    /// at most `AVATAR_CHECKS_PER_SYNC` checks: missing photos first, then
    /// the longest unchecked. Returns whether anything changed and whether
    /// more are due (for another sync soon).
    async fn sync_person_avatars(
        &self,
        manager: &SignalManager,
        conversations: &[signal_store::Conversation],
        dir: &std::path::Path,
    ) -> (bool, bool) {
        let saved: HashMap<String, String> = self.db.person_avatars().await.unwrap_or_default().into_iter().collect();
        let me = self.self_aci();
        let mut people = std::collections::BTreeSet::new();
        for conversation in conversations {
            match conversation.kind {
                ConversationKind::Direct => people.extend(conversation.id.as_direct().map(str::to_owned)),
                ConversationKind::Group => people.extend(conversation.members.iter().cloned()),
                ConversationKind::NoteToSelf => {}
            }
        }
        // Ours too (My Story shows it), with our own key.
        people.insert(me.clone());
        // The Signal chat's account is made up: it has no profile.
        people.remove(signal_store::SIGNAL_ACI);
        let now = now_millis();
        // Group members' profile keys, read once (a profile can't be read
        // without one).
        let group_keys: HashMap<Aci, ProfileKey> = self
            .store
            .groups()
            .await
            .map(|groups| groups.flatten().flat_map(|(_, g)| g.members.into_iter().map(|m| (m.aci, m.profile_key))).collect())
            .unwrap_or_default();
        let mut due = Vec::new();
        let mut keyless = 0usize;
        for sid in people {
            let has = saved.get(&sid).is_some_and(|p| std::path::Path::new(p).exists());
            // No key, no photo: left out until one arrives, rather than
            // filling every batch and starving those after them.
            let Some(aci) = Aci::parse_from_service_id_string(&sid) else { continue };
            let has_key = sid == me
                || group_keys.contains_key(&aci)
                || matches!(self.store.profile_key(&ServiceId::from(aci)).await, Ok(Some(_)));
            if !has_key {
                keyless += 1;
                continue;
            }
            let checked: i64 = self
                .db
                .setting(&format!("{SETTING_AVATAR_CHECKED}{sid}"))
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if now - checked >= AVATAR_CHECK_INTERVAL_MS {
                due.push((has, checked, sid));
            }
        }
        due.sort();
        let more = due.len() > AVATAR_CHECKS_PER_SYNC;
        let mut changed = false;
        let mut outcomes: HashMap<&'static str, usize> = HashMap::new();
        let checking = due.len().min(AVATAR_CHECKS_PER_SYNC);
        for (_, _, sid) in due.into_iter().take(AVATAR_CHECKS_PER_SYNC) {
            let outcome = self.refresh_person_avatar(manager, &sid, saved.get(&sid).map(String::as_str), dir).await;
            changed |= outcome == PhotoOutcome::Changed;
            *outcomes.entry(outcome.label()).or_default() += 1;
        }
        if checking > 0 || keyless > 0 {
            info!(checked = checking, ?outcomes, no_profile_key = keyless, more, "profile photos");
        }
        (changed, more)
    }

    /// Fetches their profile and brings their photo up to date: downloaded
    /// when its avatar path differs from the one saved, removed when they
    /// have none. Kept for their 1:1 chat and wherever they appear.
    async fn refresh_person_avatar(
        &self,
        manager: &SignalManager,
        sid: &str,
        saved: Option<&str>,
        dir: &std::path::Path,
    ) -> PhotoOutcome {
        let Some(aci) = Aci::parse_from_service_id_string(sid) else { return PhotoOutcome::NoKey };
        let ours = sid == self.self_aci();
        // Ours is our own; someone known only from a group has theirs in
        // the group.
        let key = if ours { Some(self.our_profile_key(manager).await) } else { self.profile_key_of(&ServiceId::from(aci)).await };
        let Some(key) = key else { return PhotoOutcome::NoKey };
        // Failures count as a check too, so a profile we can't read (or a
        // rate limit) isn't asked for again until tomorrow.
        let _ = self.db.set_setting(&format!("{SETTING_AVATAR_CHECKED}{sid}"), &now_millis().to_string()).await;
        let remote = match crate::profiles::fetch(manager, aci, key).await {
            Ok(remote) => remote,
            Err(error) => {
                debug!(error = %format!("{error:#}"), member = %sid, "profile fetch failed");
                return PhotoOutcome::ProfileFailed;
            }
        };
        // The same fetch brings their name, about and badges up to date.
        self.save_remote_profile(sid, &remote).await;
        self.apply_person_photo(manager, sid, remote.avatar, key, saved, dir).await
    }

    /// Brings their saved photo in line with `path`, their profile's
    /// avatar: downloaded when it differs from the one saved, removed when
    /// there's none. Kept for their 1:1 chat and wherever they appear.
    async fn apply_person_photo(
        &self,
        manager: &SignalManager,
        sid: &str,
        path: Option<String>,
        key: ProfileKey,
        saved: Option<&str>,
        dir: &std::path::Path,
    ) -> PhotoOutcome {
        // Our "chat" is Note to Self, which keeps its own icon (Desktop's).
        let chat = if sid == self.self_aci() { None } else { Some(ConversationId::direct(sid)) };
        let Some(path) = path else {
            // No photo: drop ours, and their chat's (which can predate ours).
            let chat_had = match &chat {
                Some(chat) => self.db.conversation(chat).await.ok().flatten().and_then(|c| c.avatar_path),
                None => None,
            };
            if saved.is_none() && chat_had.is_none() {
                return PhotoOutcome::NoPhoto;
            }
            for old in saved.into_iter().chain(chat_had.as_deref()) {
                let _ = std::fs::remove_file(old);
            }
            let _ = self.db.remove_person_avatar(sid).await;
            if let Some(chat) = &chat {
                let _ = self.db.set_avatar(chat, None).await;
            }
            return PhotoOutcome::Changed;
        };
        let stem = crate::profiles::file_stem(sid, &path);
        let current = saved.map(std::path::Path::new).filter(|p| p.exists());
        if let Some(current) = current.filter(|p| p.file_stem().is_some_and(|s| s == stem.as_str())) {
            // Saved while they were only in a group: their 1:1 chat gets it too.
            let chat_has = match &chat {
                Some(chat) => self.db.conversation(chat).await.ok().flatten().map(|c| c.avatar_path),
                None => None,
            };
            if let (Some(None), Some(chat)) = (chat_has, &chat)
                && self.db.set_avatar(chat, current.to_str()).await.is_ok()
            {
                return PhotoOutcome::Changed;
            }
            return PhotoOutcome::Unchanged;
        }
        let bytes = match crate::profiles::download_avatar(manager, &path, key).await {
            Ok(bytes) => bytes,
            Err(error) => {
                debug!(error = %format!("{error:#}"), member = %sid, "profile photo download failed");
                return PhotoOutcome::DownloadFailed;
            }
        };
        let ext = if bytes.starts_with(b"\x89PNG") { "png" } else { "jpg" };
        let file = dir.join(format!("{stem}.{ext}"));
        if let Err(error) = std::fs::write(&file, &bytes) {
            warn!(%error, "failed to save avatar");
            return PhotoOutcome::DownloadFailed;
        }
        let Some(file) = file.to_str() else { return PhotoOutcome::DownloadFailed };
        if let Some(old) = current.filter(|old| *old != std::path::Path::new(file)) {
            let _ = std::fs::remove_file(old);
        }
        if let Some(chat) = &chat {
            let _ = self.db.set_avatar(chat, Some(file)).await;
        }
        match self.db.set_person_avatar(sid, file).await {
            Ok(()) => PhotoOutcome::Changed,
            Err(_) => PhotoOutcome::DownloadFailed,
        }
    }

    /// Brings a group's photo up to date: downloaded when the group's avatar
    /// (a CDN path, new for each upload) differs from the one we saved,
    /// removed when the group has none. `fetch` asks the group server rather
    /// than presage's copy of the group, for right after a change.
    async fn refresh_group_avatar(&self, manager: &SignalManager, key: [u8; 32], fetch: bool) -> bool {
        let conversation = ConversationId::group(&key);
        let stored = if fetch { None } else { self.store.group(key).await.ok().flatten().map(|g| g.avatar) };
        let failed_key = format!("{SETTING_GROUP_FETCH_FAILED}{}", conversation.0);
        let path = match stored {
            Some(path) => path,
            None => {
                // A group the server wouldn't give us (one we're no longer
                // in) is asked again a day later, not every sync.
                let failed_at: i64 =
                    self.db.setting(&failed_key).await.ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
                if !fetch && now_millis() - failed_at < AVATAR_CHECK_INTERVAL_MS {
                    return false;
                }
                let fetched = async {
                    let group = crate::groups::GroupSession::new(manager, key).await?.fetch().await?;
                    let path = group.avatar.clone();
                    self.store.save_group(key, group).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                    anyhow::Ok(path)
                }
                .await;
                match fetched {
                    Ok(path) => path,
                    Err(error) => {
                        warn!(error = %format!("{error:#}"), "couldn't fetch a group for its photo (asking again tomorrow)");
                        let _ = self.db.set_setting(&failed_key, &now_millis().to_string()).await;
                        return false;
                    }
                }
            }
        };
        let saved = self.db.conversation(&conversation).await.ok().flatten().and_then(|c| c.avatar_path);
        if path.is_empty() {
            let Some(saved) = saved else { return false };
            let _ = std::fs::remove_file(&saved);
            return self.db.set_avatar(&conversation, None).await.is_ok();
        }
        // The file is named after the avatar it holds, so a new one shows.
        let name = format!("{}-{}", conversation.0.replace(':', "-"), &hex::encode(<sha2::Sha256 as sha2::Digest>::digest(path.as_bytes()))[..16]);
        let current = saved.as_deref().map(std::path::Path::new);
        if current.is_some_and(|p| p.exists() && p.file_stem().is_some_and(|stem| stem == name.as_str())) {
            return false;
        }
        let bytes = match crate::groups::download_avatar(manager, key, &path).await {
            Ok(Some(bytes)) if !bytes.is_empty() => bytes,
            Ok(_) => return false,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "group photo download failed");
                return false;
            }
        };
        let ext = if bytes.starts_with(b"\x89PNG") { "png" } else { "jpg" };
        let file = self.config.data_dir.join("avatars").join(format!("{name}.{ext}"));
        if let Err(error) = std::fs::write(&file, &bytes) {
            warn!(%error, "failed to save avatar");
            return false;
        }
        if let Some(old) = current.filter(|old| *old != file) {
            let _ = std::fs::remove_file(old);
        }
        self.db.set_avatar(&conversation, file.to_str()).await.is_ok()
    }

    async fn title_for(self: &Rc<Self>, conversation: &ConversationId) -> Option<ConversationUpdate> {
        if let Some(key) = conversation.as_group() {
            let key: [u8; 32] = key.try_into().ok()?;
            let group = self.store.group(key).await.ok().flatten()?;
            self.group_revisions.borrow_mut().insert(conversation.clone(), group.revision);
            // Signal colors a group by its group id, and its members' names
            // by their place among the sorted member ACIs.
            let id = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key)).get_group_identifier();
            let mut members: Vec<String> = group.members.iter().map(|m| m.aci.service_id_string()).collect();
            members.sort();
            let me = self.self_aci();
            // Our stored copy of a group we've left still lists us.
            let stored = self.db.conversation(conversation).await.ok().flatten();
            let left = stored.as_ref().is_some_and(|c| c.left);
            let ended = stored.as_ref().is_some_and(|c| c.ended);
            // Desktop's `canEditGroupInfo`: never in an ended group.
            let can_edit_info = !left && !ended && mapping::can_edit_group_info(&group, &me);
            let admins: Vec<String> = group
                .members
                .iter()
                .filter(|m| m.role == presage::libsignal_service::groups_v2::Role::Administrator)
                .map(|m| m.aci.service_id_string())
                .collect();
            let member_labels = group
                .members
                .iter()
                .filter_map(|m| {
                    let text = m.label.clone().filter(|t| !t.trim().is_empty())?;
                    Some((m.aci.service_id_string(), signal_store::MemberLabel { emoji: m.label_emoji.clone(), text }))
                })
                .collect();
            let can_label = !left && !ended && mapping::can_set_member_label(&group, &me);
            let can_add_members = !left && !ended && mapping::can_add_group_members(&group, &me);
            let pni = self.manager().map(|m| m.registration_data().service_ids.pni().service_id_string());
            let stored_state = stored.map(|c| c.group_state).unwrap_or_default();
            let group_state = mapping::group_state_of(&group, key, &me, pni.as_deref(), &stored_state);
            return Some(ConversationUpdate {
                group_state: Some(group_state),
                can_add_members: Some(can_add_members),
                kind: Some(ConversationKind::Group),
                member_labels: Some(member_labels),
                can_label: Some(can_label),
                title: Some(group.title).filter(|t| !t.trim().is_empty()),
                expire_timer: group.disappearing_messages_timer.map(|t| t.duration),
                color: Some(signal_store::avatar_color_of_bytes(&id)),
                members: Some(members),
                can_edit_info: Some(can_edit_info),
                description: Some(group.description.unwrap_or_default()),
                admins: Some(admins),
                ..Default::default()
            });
        }
        let sid = conversation.as_direct()?;
        if conversation.is_signal() {
            return Some(ConversationUpdate {
                kind: Some(ConversationKind::Direct),
                title: Some("Signal".into()),
                ..Default::default()
            });
        }
        if sid == self.self_aci() {
            return Some(ConversationUpdate {
                kind: Some(ConversationKind::NoteToSelf),
                title: Some("Note to Self".into()),
                ..Default::default()
            });
        }
        let service_id = ServiceId::parse_from_service_id_string(sid)?;
        let contact = self.store.contact_by_id(&service_id).await.ok().flatten();
        let phone = contact.as_ref().and_then(|c| c.phone_number.as_ref()).map(|p| p.to_string());
        // No name known (yet): keep a title we already have (one from
        // transferred history, say) rather than fall back to a number.
        let existing = self.db.conversation(conversation).await.ok().flatten().map(|c| c.title).filter(|t| !t.is_empty());
        let title = match self.name_of(sid).await {
            Some(name) => Some(name),
            None if existing.is_some() => None,
            None => Some(phone.clone().unwrap_or_else(|| short_id(sid))),
        };
        Some(ConversationUpdate {
            kind: Some(ConversationKind::Direct),
            title,
            phone,
            expire_timer: None,
            color: Some(signal_store::avatar_color(sid)),
            members: None,
            can_edit_info: Some(true),
            description: None,
            admins: None,
            member_labels: None,
            can_label: None,
            can_add_members: None,
            ended: None,
            group_state: None,
            added_by: None,
        })
    }

    async fn ensure_title(self: &Rc<Self>, conversation: &ConversationId) {
        if self.titled.borrow().contains(conversation) {
            return;
        }
        // A group's state is worked out from what's stored: hold it still.
        let _lock = match conversation.as_group() {
            Some(_) => Some(self.group_state_lock(conversation).await),
            None => None,
        };
        if let Some(update) = self.title_for(conversation).await {
            if let Err(error) = self.db.upsert_conversation(conversation, update).await {
                self.error("failed to update conversation", error);
                return;
            }
            self.titled.borrow_mut().insert(conversation.clone());
        }
    }

    async fn refresh_titles(self: &Rc<Self>) {
        let mut all = self.db.conversations().await.unwrap_or_default();
        all.extend(self.db.archived_conversations().await.unwrap_or_default());
        let mut changed = false;
        for conversation in all {
            if self.titled.borrow().contains(&conversation.id) {
                continue;
            }
            let _lock = match conversation.id.as_group() {
                Some(_) => Some(self.group_state_lock(&conversation.id).await),
                None => None,
            };
            if let Some(update) = self.title_for(&conversation.id).await {
                let differs = update.title.as_ref().is_some_and(|t| *t != conversation.title)
                    || update.kind.is_some_and(|k| k != conversation.kind)
                    || (update.phone.is_some() && update.phone != conversation.phone)
                    || update.color.is_some_and(|c| c != conversation.color)
                    || update.members.as_ref().is_some_and(|m| *m != conversation.members)
                    || update.expire_timer.is_some_and(|t| t != conversation.expire_timer)
                    || update.can_edit_info.is_some_and(|v| v != conversation.can_edit_info)
                    || update.can_label.is_some_and(|v| v != conversation.can_label)
                    || update.can_add_members.is_some_and(|v| v != conversation.can_add_members)
                    || (update.added_by.is_some() && update.added_by != conversation.added_by)
                    || update.admins.as_ref().is_some_and(|a| *a != conversation.admins)
                    || update.member_labels.as_ref().is_some_and(|l| *l != conversation.member_labels)
                    || update.group_state.as_ref().is_some_and(|g| *g != conversation.group_state)
                    || update.description.as_ref().is_some_and(|d| Some(d).filter(|d| !d.is_empty()) != conversation.description.as_ref());
                if differs {
                    changed = true;
                    if let Err(error) = self.db.upsert_conversation(&conversation.id, update).await {
                        self.error("failed to update conversation", error);
                        continue;
                    }
                }
                self.titled.borrow_mut().insert(conversation.id);
            }
        }
        if changed {
            self.emit(Event::ConversationsChanged);
        }
    }

    async fn resolve_thread(&self, manager: &SignalManager, thread: &mapping::ThreadRef) -> Option<ConversationId> {
        match thread {
            mapping::ThreadRef::Direct(sid) => Some(ConversationId::direct(sid)),
            mapping::ThreadRef::GroupId(id) => self.group_for_id(manager, id).await.map(|key| ConversationId::group(&key)),
        }
    }

    async fn remove_message(&self, conversation: &ConversationId, message_id: i64) {
        // An unviewed view-once message's media goes with it.
        self.erase_view_once(message_id, signal_store::ViewOnce::Gone).await;
        match self.db.remove_message(message_id).await {
            Ok(_) => {
                self.emit(Event::MessageRemoved { conversation: conversation.clone(), message_id });
                self.emit(Event::ConversationsChanged);
            }
            Err(error) => self.error("failed to delete message", error),
        }
    }

    async fn remove_conversation(&self, conversation: &ConversationId) {
        match self.db.remove_conversation(conversation).await {
            Ok(()) => {
                self.emit(Event::ConversationRemoved(conversation.clone()));
                self.emit(Event::ConversationsChanged);
            }
            Err(error) => self.error("failed to delete chat", error),
        }
    }

    fn is_note_to_self(&self, conversation: &ConversationId) -> bool {
        conversation.as_direct() == Some(self.self_aci().as_str())
    }

    /// Whether `aci` is an admin of the group `conversation`, by presage's
    /// copy of the group (Desktop's `isMemberGroupAdmin`).
    async fn is_group_admin(&self, conversation: &ConversationId, aci: &str) -> bool {
        use presage::libsignal_service::groups_v2::Role;
        let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return false };
        let Some(group) = self.store.group(key).await.ok().flatten() else { return false };
        group.members.iter().any(|m| m.role == Role::Administrator && m.aci.service_id_string() == aci)
    }

    /// The newest of `named` that we have.
    async fn newest_of(&self, conversation: &ConversationId, named: &[(Option<String>, i64)]) -> Option<signal_store::Message> {
        let mut newest: Option<signal_store::Message> = None;
        for (author, sent_at) in named {
            let Ok(Some(found)) = self.db.find_message(conversation, author.as_deref(), *sent_at).await else { continue };
            if newest.as_ref().is_none_or(|n| (found.received_at, found.id) > (n.received_at, n.id)) {
                newest = Some(found);
            }
        }
        newest
    }

    /// A chat deleted on another device: up to the newest message it named,
    /// then the chat itself if nothing newer arrived here (as Desktop).
    async fn delete_chat_from_sync(&self, conversation: &ConversationId, chat: &mapping::ChatDelete) {
        let newest = match self.newest_of(conversation, &chat.most_recent).await {
            Some(newest) => Some(newest),
            None => self.newest_of(conversation, &chat.most_recent_non_expiring).await,
        };
        let left = match newest {
            Some(newest) => match self.db.remove_messages_through(conversation, newest.id).await {
                Ok(left) => left,
                Err(error) => return self.error("failed to delete chat", error),
            },
            None => self.db.messages(conversation, None, 1).await.map_or(1, |m| m.len() as i64),
        };
        if chat.full && left == 0 {
            self.remove_conversation(conversation).await;
        } else {
            // Rows went; reload what's open.
            self.emit(Event::ConversationCleared(conversation.clone()));
            self.emit(Event::ConversationsChanged);
        }
    }

    /// How sync messages name a chat (groups by group id, not master key).
    fn conversation_identifier(conversation: &ConversationId) -> Option<presage::proto::ConversationIdentifier> {
        use presage::proto::conversation_identifier::Identifier;
        let identifier = match conversation.as_group() {
            Some(key) => {
                let key: [u8; 32] = key.try_into().ok()?;
                let params = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key));
                Identifier::ThreadGroupId(params.get_group_identifier().to_vec())
            }
            None => match crate::remote_config::current().wire_service_id(conversation.as_direct()?) {
                WireServiceId::String(s) => Identifier::ThreadServiceId(s),
                WireServiceId::Binary(b) => Identifier::ThreadServiceIdBinary(b),
            },
        };
        Some(presage::proto::ConversationIdentifier { identifier: Some(identifier) })
    }

    fn addressable(&self, message: &signal_store::Message) -> presage::proto::AddressableMessage {
        use presage::proto::addressable_message::Author;
        presage::proto::AddressableMessage {
            sent_timestamp: Some(message.sent_at as u64),
            author: Some(
                match crate::remote_config::current()
                    .wire_service_id(message.author.as_deref().unwrap_or(&self.self_aci()))
                {
                    WireServiceId::String(s) => Author::AuthorServiceId(s),
                    WireServiceId::Binary(b) => Author::AuthorServiceIdBinary(b),
                },
            ),
        }
    }

    fn sync_delete(&self, delete: sync_message::DeleteForMe) {
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::DeleteForMe(delete)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Removes a message here and tells our other devices (Signal's
    /// "delete for me").
    async fn delete_for_me(self: Rc<Self>, message_ids: Vec<i64>) {
        // One sync message, grouped by chat, as Desktop sends.
        let mut by_chat: Vec<(ConversationId, Vec<presage::proto::AddressableMessage>)> = Vec::new();
        for message_id in message_ids {
            let Some(message) = self.db.message(message_id).await.ok().flatten() else { continue };
            let conversation = message.conversation_id.clone();
            self.remove_message(&conversation, message_id).await;
            let addressable = self.addressable(&message);
            match by_chat.iter_mut().find(|(c, _)| *c == conversation) {
                Some((_, messages)) => messages.push(addressable),
                None => by_chat.push((conversation, vec![addressable])),
            }
        }
        let message_deletes: Vec<_> = by_chat
            .into_iter()
            .filter_map(|(conversation, messages)| {
                Some(sync_message::delete_for_me::MessageDeletes {
                    conversation: Some(Self::conversation_identifier(&conversation)?),
                    messages,
                })
            })
            .collect();
        if !message_deletes.is_empty() {
            self.sync_delete(sync_message::DeleteForMe { message_deletes, ..Default::default() });
        }
    }

    /// Signal's edit: the new text replaces the old everywhere, aimed at the
    /// original's timestamp (every edit is). Attachments can't change.
    async fn edit_message(
        self: Rc<Self>,
        message_id: i64,
        body: String,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
    ) {
        let wire_body = body.trim_end().to_owned();
        let wire_styles = mapping::wire_styles(&wire_body, &styles);
        let (body, spans, style_spans) = self.render_body(&wire_body, &mentions, &wire_styles).await;
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        let conversation = message.conversation_id.clone();
        if body.is_empty() || (body == message.body && spans == message.mentions && style_spans == message.styles) {
            return;
        }
        if !crate::edits::can_edit(&message, self.is_note_to_self(&conversation), now_millis()) {
            return self.error("can't edit", "messages can be edited for 24 hours, up to 10 times");
        }
        let timestamp = self.next_timestamp() as u64;
        if let Err(error) = self.db.edit_message(message_id, &body, timestamp as i64).await {
            return self.error("failed to edit message", error);
        }
        let _ = self.db.set_mentions(message_id, &spans).await;
        let _ = self.db.set_styles(message_id, &style_spans).await;
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id });
        self.emit(Event::ConversationsChanged);
        let self_aci = self.self_aci();
        let config = crate::remote_config::current();
        let mut body_ranges = mapping::mention_ranges(&wire_body, &mentions, &config);
        body_ranges.extend(mapping::style_ranges(&wire_styles));
        let data = DataMessage {
            body_ranges,
            body: Some(wire_body),
            timestamp: Some(timestamp),
            quote: message.quote.as_ref().map(|q| {
                let author = config.wire_service_id(q.author.as_deref().unwrap_or(&self_aci));
                data_message::Quote {
                    id: Some(q.sent_at as u64),
                    author_aci: author.string(),
                    author_aci_binary: author.binary(),
                    text: Some(q.text.clone()),
                    attachments: mapping::quoted_attachments(q.attachment.as_ref()),
                    r#type: Some(data_message::quote::Type::Normal.into()),
                    ..Default::default()
                }
            }),
            group_v2: self.group_context(&conversation).await,
            ..Default::default()
        };
        let edit = presage::proto::EditMessage {
            target_sent_timestamp: Some(message.sent_at as u64),
            data_message: Some(data),
        };
        self.enqueue(Outbound::Send { conversation, body: ContentBody::EditMessage(edit), timestamp, message: None });
    }

    /// Sends a failed message again from what we stored, with its original
    /// timestamp (so recipients who did get it de-duplicate), as Desktop's
    /// "Retry send".
    async fn retry_send(self: Rc<Self>, message_id: i64) {
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        if message.direction != Direction::Outgoing || message.attachments.iter().any(|a| a.sticker) {
            if message.attachments.iter().any(|a| a.sticker) {
                self.emit(Event::Error("A sticker can't be retried; send it again from the picker".into()));
            }
            return;
        }
        if !matches!(self.db.mark_resending(message_id).await, Ok(true)) {
            return;
        }
        let conversation = message.conversation_id.clone();
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id });
        let timestamp = message.sent_at as u64;
        let self_aci = self.self_aci();
        let config = crate::remote_config::current();
        // Mentions go back to placeholders and ranges, as first sent.
        let (wire_body, mentioned) = mapping::placeholders_for(&message.body, &message.mentions);
        let data = DataMessage {
            body_ranges: mapping::mention_ranges(&wire_body, &mentioned, &config),
            body: Some(wire_body).filter(|b| !b.is_empty()),
            timestamp: Some(timestamp),
            quote: message.quote.as_ref().map(|q| {
                let author = config.wire_service_id(q.author.as_deref().unwrap_or(&self_aci));
                data_message::Quote {
                    id: Some(q.sent_at as u64),
                    author_aci: author.string(),
                    author_aci_binary: author.binary(),
                    text: Some(q.text.clone()),
                    attachments: mapping::quoted_attachments(q.attachment.as_ref()),
                    r#type: Some(data_message::quote::Type::Normal.into()),
                    ..Default::default()
                }
            }),
            group_v2: self.group_context(&conversation).await,
            ..Default::default()
        };
        let mut data = data;
        polls::poll_data(&mut data, &message);
        // Ours stays view-once until it's sent.
        if message.view_once == Some(signal_store::ViewOnce::Ready) {
            data.is_view_once = Some(true);
        }
        self.enqueue_with_media(conversation, message_id, data, timestamp, &message.attachments, message.link_preview.as_ref());
    }

    /// Queues a stored message's `data` with its media: a GIF, photos (from
    /// our local copies) and the link preview's image.
    fn enqueue_with_media(
        &self,
        conversation: ConversationId,
        message_id: i64,
        mut data: DataMessage,
        timestamp: u64,
        attachments: &[AttachmentMeta],
        preview: Option<&LinkPreview>,
    ) {
        let local = |a: &AttachmentMeta| a.local_path.as_ref().map(std::path::PathBuf::from);
        if let Some(gif) = attachments.iter().find(|a| a.gif)
            && let Some(path) = local(gif)
        {
            let size = (gif.width.unwrap_or(0), gif.height.unwrap_or(0));
            return self.enqueue(Outbound::SendGif { conversation, body: data, timestamp, message: message_id, path, size });
        }
        let images: Vec<_> = attachments
            .iter()
            .filter_map(|a| {
                Some(Upload {
                    path: local(a)?,
                    file_name: a.file_name.clone(),
                    // Only the kinds we optimize go through it (a HEIC or TIFF
                    // sent as a file stays as it is).
                    file_type: (!matches!(a.content_type.as_str(), "image/png" | "image/jpeg" | "image/webp" | "image/gif"))
                        .then(|| a.content_type.clone()),
                    voice: a.voice,
                })
            })
            .collect();
        let preview_image = preview.and_then(|p| p.image.as_ref()).and_then(local);
        if let Some(preview) = preview {
            data.preview = vec![presage::proto::Preview {
                url: Some(preview.url.clone()),
                title: preview.title.clone(),
                description: preview.description.clone(),
                date: preview.date.map(|d| d as u64),
                image: None,
            }];
        }
        if images.is_empty() && preview.is_none() {
            self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp, message: Some(message_id) });
        } else {
            self.enqueue(Outbound::SendImages { conversation, body: data, timestamp, message: message_id, images, preview_image });
        }
    }

    /// Desktop's "Forward": a new message per chat with the same text and
    /// media. Attachments we haven't downloaded can't go along.
    async fn forward(self: Rc<Self>, message_ids: Vec<i64>, conversations: Vec<ConversationId>) {
        // Desktop's `maybeForwardMessages` refuses ended groups.
        for conversation in &conversations {
            if self.is_ended(conversation).await {
                return self.error("can't forward", "the group has ended");
            }
        }
        let mut skipped = 0;
        for conversation in &conversations {
            for message_id in &message_ids {
                let Some(message) = self.db.message(*message_id).await.ok().flatten() else { continue };
                let attachments: Vec<AttachmentMeta> =
                    message.attachments.iter().filter(|a| a.local_path.is_some() && !a.sticker).cloned().collect();
                // Desktop doesn't forward polls or view-once messages
                // (`canForward`).
                if message.deleted
                    || message.poll.is_some()
                    || message.view_once.is_some()
                    || attachments.len() < message.attachments.len()
                    || (message.body.is_empty() && attachments.is_empty())
                {
                    skipped += 1;
                    continue;
                }
                let Some((id, data, timestamp)) = self
                    .store_outgoing(conversation, message.body.clone(), None, attachments.clone(), message.link_preview.clone())
                    .await
                else {
                    continue;
                };
                self.enqueue_with_media(conversation.clone(), id, data, timestamp, &attachments, message.link_preview.as_ref());
            }
        }
        if skipped > 0 {
            self.emit(Event::Error(
                "Some messages couldn't be forwarded: polls, stickers and view-once media can't be, nor attachments that haven't downloaded yet".into(),
            ));
        }
    }

    /// Removes a chat here and on our other devices. Like Desktop, it names
    /// the newest messages so a device that has newer ones keeps them.
    async fn delete_conversation(self: Rc<Self>, conversation: ConversationId) {
        // Newest first; Desktop names five, and five without a timer too
        // when some have one.
        // Real messages only: other devices don't have our notices.
        let recent: Vec<_> = self
            .db
            .messages(&conversation, None, 500)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.notice.is_none())
            .collect();
        self.remove_conversation(&conversation).await;
        let Some(identifier) = Self::conversation_identifier(&conversation) else { return };
        let most_recent: Vec<_> = recent.iter().rev().take(5).map(|m| self.addressable(m)).collect();
        let delete = if most_recent.is_empty() {
            sync_message::DeleteForMe {
                local_only_conversation_deletes: vec![sync_message::delete_for_me::LocalOnlyConversationDelete {
                    conversation: Some(identifier),
                }],
                ..Default::default()
            }
        } else {
            let disappearing = recent.iter().rev().take(5).any(|m| m.expires_in > 0);
            let non_expiring = if disappearing {
                recent.iter().rev().filter(|m| m.expires_in == 0).take(5).map(|m| self.addressable(m)).collect()
            } else {
                Vec::new()
            };
            sync_message::DeleteForMe {
                conversation_deletes: vec![sync_message::delete_for_me::ConversationDelete {
                    conversation: Some(identifier),
                    most_recent_messages: most_recent,
                    is_full_delete: Some(true),
                    most_recent_non_expiring_messages: non_expiring,
                }],
                ..Default::default()
            }
        };
        self.sync_delete(delete);
    }

    async fn group_for_id(&self, manager: &SignalManager, id: &[u8]) -> Option<[u8; 32]> {
        if let Some(key) = self.group_ids.borrow().get(id) {
            return Some(*key);
        }
        let groups = manager.store().groups().await.ok()?;
        let mut map = self.group_ids.borrow_mut();
        for (key, _) in groups.flatten() {
            let params = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key));
            map.insert(params.get_group_identifier().to_vec(), key);
        }
        map.get(id).copied()
    }

    // ---------------------------------------------------------------- sending

    async fn send_text(
        self: Rc<Self>,
        conversation: ConversationId,
        body: String,
        quote: Option<i64>,
        preview: Option<crate::LinkPreviewDraft>,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
    ) {
        let wire_body = body.trim_end().to_owned();
        if wire_body.is_empty() {
            return;
        }
        // We see "@Name"; the message carries placeholders and ranges.
        let wire_styles = mapping::wire_styles(&wire_body, &styles);
        let (body, spans, style_spans) = self.render_body(&wire_body, &mentions, &wire_styles).await;
        // Only for a link that's still in the message.
        let preview = preview.filter(|p| crate::link_preview::is_valid(&body, &p.url));
        let mut preview_image = None;
        let link_preview = match &preview {
            Some(draft) => {
                let image = match &draft.image {
                    Some(image) => {
                        let ext = media::ImageKind::sniff(&image.bytes).map_or("img", |k| k.extension());
                        let path = self.config.data_dir.join("attachments").join(format!("sent-{}-preview.{ext}", now_millis()));
                        if let Err(error) = tokio::fs::write(&path, &*image.bytes).await {
                            warn!(%error, "failed to save link preview image");
                            None
                        } else {
                            preview_image = Some(path.clone());
                            Some(AttachmentMeta {
                                content_type: image.content_type.into(),
                                file_name: None,
                                size: image.bytes.len() as u64,
                                width: Some(image.width),
                                height: Some(image.height),
                                local_path: Some(path.to_string_lossy().into_owned()),
                                sticker: false,
                                gif: false,
                                voice: false,
                                remote: None,
                                thumbnail_path: None,
                            })
                        }
                    }
                    None => None,
                };
                Some(LinkPreview {
                    url: draft.url.clone(),
                    title: Some(draft.title.clone()),
                    description: draft.description.clone(),
                    date: draft.date,
                    image,
                })
            }
            None => None,
        };
        let Some((id, mut data, timestamp)) =
            self.store_outgoing(&conversation, body, quote, Vec::new(), link_preview.clone()).await
        else {
            return;
        };
        self.add_ranges(id, &mut data, wire_body, &mentions, &spans, &wire_styles, &style_spans).await;
        match link_preview {
            Some(preview) => {
                data.preview = vec![presage::proto::Preview {
                    url: Some(preview.url),
                    title: preview.title,
                    description: preview.description,
                    date: preview.date.map(|d| d as u64),
                    image: None,
                }];
                self.enqueue(Outbound::SendImages {
                    conversation,
                    body: data,
                    timestamp,
                    message: id,
                    images: Vec::new(),
                    preview_image,
                });
            }
            None => self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp, message: Some(id) }),
        }
    }

    /// Saves the images next to downloaded attachments so the message shows
    /// at once; optimizing and uploading happen in the outbound queue.
    /// Desktop's view-once send: one photo or video, nothing else.
    async fn send_view_once(self: Rc<Self>, conversation: ConversationId, media: crate::OutgoingViewOnce) {
        match media {
            crate::OutgoingViewOnce::Image(image) => {
                self.send_images_as(conversation, vec![image], String::new(), None, Vec::new(), Vec::new(), true).await
            }
            crate::OutgoingViewOnce::Video(file) => {
                if !mapping::can_view_once(media::content_type_for_file(&file.file_name)) {
                    return self.error("can't send view-once", "only photos and videos can be viewed once");
                }
                self.send_file_as(conversation, file, String::new(), None, Vec::new(), Vec::new(), true).await
            }
        }
    }

    async fn send_images(
        self: Rc<Self>,
        conversation: ConversationId,
        images: Vec<crate::OutgoingImage>,
        caption: String,
        quote: Option<i64>,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
    ) {
        self.send_images_as(conversation, images, caption, quote, mentions, styles, false).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_images_as(
        self: Rc<Self>,
        conversation: ConversationId,
        images: Vec<crate::OutgoingImage>,
        caption: String,
        quote: Option<i64>,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
        view_once: bool,
    ) {
        let stamp = now_millis();
        let dir = self.config.data_dir.join("attachments");
        let mut metas = Vec::new();
        let mut files = Vec::new();
        for (ix, image) in images.into_iter().enumerate() {
            let bytes = image.bytes.as_ref().clone();
            let bytes = match tokio::task::spawn_blocking(move || media::normalize(bytes)).await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(error)) => return self.error("can't send image", format!("{error:#}")),
                Err(error) => return self.error("can't send image", error),
            };
            if crate::too_big_to_send(bytes.len()) {
                let limit = crate::size_limit_label(crate::remote_config().attachment_max_bytes());
                return self.error("can't send image", format!("larger than Signal's {limit} limit"));
            }
            let Some((kind, width, height)) = media::probe(&bytes) else {
                return self.error("can't send file", "only PNG, JPEG, GIF and WebP images can be sent");
            };
            let path = dir.join(format!("sent-{stamp}-{ix}.{}", kind.extension()));
            if let Err(error) = tokio::fs::write(&path, &bytes).await {
                return self.error("failed to save image", error);
            }
            metas.push(AttachmentMeta {
                content_type: kind.content_type().into(),
                file_name: image.file_name.clone(),
                size: bytes.len() as u64,
                width: Some(width),
                height: Some(height),
                local_path: Some(path.to_string_lossy().into_owned()),
                sticker: false,
                gif: false,
                voice: false,
                remote: None,
                thumbnail_path: None,
            });
            files.push(Upload { path, file_name: image.file_name, file_type: None, voice: false });
        }
        if files.is_empty() {
            return;
        }
        let wire_caption = caption.trim_end().to_owned();
        let wire_styles = mapping::wire_styles(&wire_caption, &styles);
        let (caption, spans, style_spans) = self.render_body(&wire_caption, &mentions, &wire_styles).await;
        let Some((id, mut data, timestamp)) =
            self.store_outgoing_as(&conversation, caption, quote, metas, None, view_once).await
        else {
            return;
        };
        self.add_ranges(id, &mut data, wire_caption, &mentions, &spans, &wire_styles, &style_spans).await;
        self.enqueue(Outbound::SendImages {
            conversation,
            body: data,
            timestamp,
            message: id,
            images: files,
            preview_image: None,
        });
    }

    /// Sends a file as it is. A copy is kept next to downloaded attachments,
    /// under its own name, so it opens from the message.
    async fn send_file(
        self: Rc<Self>,
        conversation: ConversationId,
        file: crate::OutgoingFile,
        caption: String,
        quote: Option<i64>,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
    ) {
        self.send_file_as(conversation, file, caption, quote, mentions, styles, false).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_file_as(
        self: Rc<Self>,
        conversation: ConversationId,
        file: crate::OutgoingFile,
        caption: String,
        quote: Option<i64>,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
        view_once: bool,
    ) {
        if crate::too_big_to_send(file.bytes.len()) {
            let limit = crate::size_limit_label(crate::remote_config().attachment_max_bytes());
            return self.error("can't send file", format!("larger than Signal's {limit} limit"));
        }
        let name = media::safe_file_name(&file.file_name);
        let dir = self.config.data_dir.join("attachments").join(format!("sent-{}", now_millis()));
        let path = dir.join(&name);
        let written = async {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::write(&path, file.bytes.as_slice()).await
        };
        if let Err(error) = written.await {
            return self.error("failed to save file", error);
        }
        // A voice message goes as the phones send theirs: AAC, unnamed.
        let meta_type =
            if file.voice { VOICE_CONTENT_TYPE.to_owned() } else { media::content_type_for_file(&name).to_owned() };
        let wire_name = (!file.voice).then(|| name.clone());
        let meta = AttachmentMeta {
            content_type: meta_type.clone(),
            file_name: wire_name.clone(),
            size: file.bytes.len() as u64,
            width: None,
            height: None,
            local_path: Some(path.to_string_lossy().into_owned()),
            sticker: false,
            gif: false,
            voice: file.voice,
            remote: None,
            thumbnail_path: None,
        };
        let wire_caption = caption.trim_end().to_owned();
        let wire_styles = mapping::wire_styles(&wire_caption, &styles);
        let (caption, spans, style_spans) = self.render_body(&wire_caption, &mentions, &wire_styles).await;
        let Some((id, mut data, timestamp)) =
            self.store_outgoing_as(&conversation, caption, quote, vec![meta], None, view_once).await
        else {
            return;
        };
        self.add_ranges(id, &mut data, wire_caption, &mentions, &spans, &wire_styles, &style_spans).await;
        self.enqueue(Outbound::SendImages {
            conversation,
            body: data,
            timestamp,
            message: id,
            images: vec![Upload { path, file_name: wire_name, file_type: Some(meta_type), voice: file.voice }],
            preview_image: None,
        });
    }

    /// Mentions and formatting in a message we're sending: stored where they
    /// render, sent as placeholders with ranges.
    #[allow(clippy::too_many_arguments)]
    async fn add_ranges(
        &self,
        id: i64,
        data: &mut DataMessage,
        wire_body: String,
        mentions: &[String],
        spans: &[signal_store::MentionSpan],
        wire_styles: &[mapping::WireStyle],
        style_spans: &[signal_store::StyleSpan],
    ) {
        if mentions.is_empty() && style_spans.is_empty() {
            return;
        }
        let mentions_saved = mentions.is_empty() || self.db.set_mentions(id, spans).await.is_ok();
        let styles_saved = style_spans.is_empty() || self.db.set_styles(id, style_spans).await.is_ok();
        if (mentions_saved || styles_saved)
            && let Ok(Some(message)) = self.db.message(id).await
        {
            // Shown before the spans were saved: show it again, highlighted.
            self.emit(Event::MessageChanged { conversation: message.conversation_id, message_id: id });
            self.emit(Event::ConversationsChanged);
        }
        data.body_ranges = mapping::mention_ranges(&wire_body, mentions, &crate::remote_config::current());
        data.body_ranges.extend(mapping::style_ranges(wire_styles));
        data.body = Some(wire_body).filter(|b| !b.is_empty());
    }

    /// Stores an outgoing message (status Sending) and builds the
    /// `DataMessage` to send for it.
    async fn store_outgoing(
        &self,
        conversation: &ConversationId,
        body: String,
        quote: Option<i64>,
        attachments: Vec<AttachmentMeta>,
        link_preview: Option<LinkPreview>,
    ) -> Option<(i64, DataMessage, u64)> {
        self.store_outgoing_as(conversation, body, quote, attachments, link_preview, false).await
    }

    /// [`store_outgoing`](Self::store_outgoing), view-once when
    /// `view_once` says.
    async fn store_outgoing_as(
        &self,
        conversation: &ConversationId,
        body: String,
        quote: Option<i64>,
        attachments: Vec<AttachmentMeta>,
        link_preview: Option<LinkPreview>,
        view_once: bool,
    ) -> Option<(i64, DataMessage, u64)> {
        let conversation = conversation.clone();
        self.maybe_apply_universal_timer(&conversation).await;
        let timestamp = self.next_timestamp();
        let quoted = match quote {
            Some(id) => self.db.message(id).await.ok().flatten(),
            None => None,
        };
        let quote = quoted.as_ref().map(|m| Quote {
            author: m.author.clone(),
            author_name: match &m.author {
                None => "You".into(),
                Some(a) => m.author_name.clone().unwrap_or_else(|| short_id(a)),
            },
            sent_at: m.sent_at,
            // A quoted spoiler stays hidden, here and for them.
            text: signal_store::hide_spoilers(&m.body, &m.styles),
            thumbnail: m.thumbnail(),
            attachment: quoted_attachment(m),
            story: false,
        });

        let expire_timer = self
            .db
            .conversation(&conversation)
            .await
            .ok()
            .flatten()
            .map(|c| c.expire_timer)
            .unwrap_or(0);
        let message = NewMessage {
            conversation_id: conversation.clone(),
            sent_at: timestamp,
            received_at: timestamp,
            direction: Direction::Outgoing,
            author: None,
            author_name: None,
            body: body.clone(),
            status: DeliveryStatus::Sending,
            quote,
            attachments,
            link_preview,
            expires_in: expire_timer,
        };
        let view_once = view_once.then_some(signal_store::ViewOnce::Ready);
        let id = match self.db.insert_message_view_once(&message, view_once).await {
            Ok(id) => id,
            Err(error) => {
                self.error("failed to store outgoing message", error);
                return None;
            }
        };
        let _ = self.db.set_draft(&conversation, None).await;
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
        self.emit(Event::ConversationsChanged);

        let self_aci = self.self_aci();
        let config = crate::remote_config::current();
        let data = DataMessage {
            body: Some(body).filter(|b| !b.is_empty()),
            timestamp: Some(timestamp as u64),
            quote: quoted.map(|m| {
                let author = config.wire_service_id(m.author.as_deref().unwrap_or(&self_aci));
                data_message::Quote {
                    id: Some(m.sent_at as u64),
                    author_aci: author.string(),
                    author_aci_binary: author.binary(),
                    text: Some(signal_store::hide_spoilers(&m.body, &m.styles)),
                    attachments: mapping::quoted_attachments(quoted_attachment(&m).as_ref()),
                    // Desktop's `makeQuote` marks a quoted poll (`isPoll`).
                    r#type: Some(
                        if m.poll.is_some() { data_message::quote::Type::Poll } else { data_message::quote::Type::Normal }
                            .into(),
                    ),
                    ..Default::default()
                }
            }),
            group_v2: self.group_context(&conversation).await,
            is_view_once: view_once.map(|_| true),
            ..Default::default()
        };
        Some((id, data, timestamp as u64))
    }

    async fn react(self: Rc<Self>, message_id: i64, emoji: String, remove: bool) {
        let Some(target) = self.db.message(message_id).await.ok().flatten() else {
            return;
        };
        let timestamp = self.next_timestamp();
        let local = if remove {
            self.db.remove_reaction(message_id, None).await
        } else {
            self.db
                .set_reaction(message_id, Reaction { emoji: emoji.clone(), author: None, sent_at: timestamp })
                .await
        };
        if let Err(error) = local {
            return self.error("failed to store reaction", error);
        }
        if !remove {
            let _ = self.db.record_emoji_use(&emoji).await;
        }
        self.emit(Event::MessageChanged { conversation: target.conversation_id.clone(), message_id });

        let target_author = crate::remote_config::current()
            .wire_service_id(target.author.as_deref().unwrap_or(&self.self_aci()));
        let data = DataMessage {
            timestamp: Some(timestamp as u64),
            reaction: Some(data_message::Reaction {
                emoji: Some(emoji),
                remove: Some(remove),
                target_author_aci: target_author.string(),
                target_author_aci_binary: target_author.binary(),
                target_sent_timestamp: Some(target.sent_at as u64),
            }),
            group_v2: self.group_context(&target.conversation_id).await,
            ..Default::default()
        };
        self.enqueue(Outbound::Send {
            conversation: target.conversation_id,
            body: data.into(),
            timestamp: timestamp as u64,
            message: None,
        });
    }

    /// Deletes a message for everyone: our own with Signal's normal delete
    /// while it's allowed, else anyone's in a group we're an admin of
    /// (Desktop's `sendDeleteForEveryoneMessage`). Our other devices learn
    /// of it from the sent transcript, as with any message we send.
    async fn delete_for_everyone(self: Rc<Self>, message_id: i64) {
        let Some(target) = self.db.message(message_id).await.ok().flatten() else {
            return;
        };
        let conversation = &target.conversation_id;
        let stored = self.db.conversation(conversation).await.ok().flatten();
        let left = stored.as_ref().is_some_and(|c| c.left);
        let chat = crate::deletes::Chat::new(
            self.is_note_to_self(conversation),
            conversation.as_group().is_some(),
            !left && self.is_group_admin(conversation, &self.self_aci()).await,
            stored.as_ref().is_some_and(|c| c.ended),
        );
        let Some(kind) = crate::deletes::send_kind(&target, chat, now_millis()) else {
            return self.error(
                "cannot delete for everyone",
                "only your own sent messages, or anyone's in a group you're an admin of, from the last 24 hours",
            );
        };
        let target_sent_timestamp = Some(target.sent_at as u64);
        let (marked, delete, admin_delete) = match kind {
            crate::deletes::Kind::Normal => (
                self.db.mark_deleted(message_id).await.map(|()| true),
                Some(data_message::Delete { target_sent_timestamp }),
                None,
            ),
            crate::deletes::Kind::Admin => {
                let author = target.author.clone().unwrap_or_else(|| self.self_aci());
                let Some(author) = Aci::parse_from_service_id_string(&author).map(|a| a.service_id_binary()) else {
                    return self.error("cannot delete for everyone", "the message's author has no ACI");
                };
                let by = signal_store::DeletedByAdmin { by: None };
                let admin_delete =
                    data_message::AdminDelete { target_author_aci_binary: Some(author), target_sent_timestamp };
                (self.db.mark_deleted_by_admin(message_id, &by).await, None, Some(admin_delete))
            }
        };
        match marked {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => return self.error("failed to delete message", error),
        }
        self.emit(Event::MessageChanged { conversation: target.conversation_id.clone(), message_id });
        self.emit(Event::ConversationsChanged);

        let timestamp = self.next_timestamp() as u64;
        let data = DataMessage {
            timestamp: Some(timestamp),
            delete,
            admin_delete,
            group_v2: self.group_context(&target.conversation_id).await,
            ..Default::default()
        };
        self.enqueue(Outbound::Send {
            conversation: target.conversation_id,
            body: data.into(),
            timestamp,
            message: None,
        });
    }

    /// An admin ended this group (Desktop's `terminated`).
    async fn is_ended(&self, conversation: &ConversationId) -> bool {
        conversation.as_group().is_some() && self.db.conversation(conversation).await.ok().flatten().is_some_and(|c| c.ended)
    }

    async fn is_blocked(&self, conversation: &ConversationId) -> bool {
        self.db.conversation(conversation).await.ok().flatten().is_some_and(|c| c.blocked)
    }

    async fn mark_read(self: Rc<Self>, conversation: ConversationId) {
        // Opening a chat marked unread clears that on every device.
        let marked = self.db.conversation(&conversation).await.ok().flatten().is_some_and(|c| c.marked_unread);
        if marked && let Some(manager) = self.manager() {
            let this = self.clone();
            let id = conversation.clone();
            tokio::task::spawn_local(async move {
                match storage::write_chat(&manager, &id, storage::ChatEdit::MarkedUnread(false)).await {
                    Ok(()) => this.storage_written(),
                    Err(error) => warn!(error = %format!("{error:#}"), "couldn't clear marked unread"),
                }
            });
        }
        let mut unread = match self.db.mark_read(&conversation).await {
            Ok(unread) => unread,
            Err(error) => return self.error("failed to mark read", error),
        };
        // Votes in our polls are read too; our other devices hear of them
        // by a read sync naming the poll, and nobody gets a receipt
        // (Desktop's `markConversationRead`).
        let poll_votes = match self.db.take_unread_poll_votes(&conversation).await {
            Ok(polls) => polls,
            Err(error) => {
                self.error("failed to mark poll votes read", error);
                Vec::new()
            }
        };
        // Release notes are only here: no receipts to the Signal chat's
        // made-up account, and nothing for our other devices to sync.
        unread.retain(|(author, _)| author != signal_store::SIGNAL_ACI);
        // Read messages start disappearing now.
        self.expiry.notify_one();
        self.emit(Event::ConversationsChanged);
        // Its calls are read too (Desktop's `markConversationRead`).
        self.mark_calls_read(Some(conversation.clone())).await;
        if unread.is_empty() && poll_votes.is_empty() {
            return;
        }

        // Not to a message request: they don't learn we've seen it until we
        // accept (Desktop's `markConversationRead` and `sendReceipts`).
        let send_receipts = self.setting_on(AccountSetting::ReadReceipts).await && self.is_accepted(&conversation).await;
        let mut by_author: HashMap<String, Vec<u64>> = HashMap::new();
        for (author, sent_at) in &unread {
            by_author.entry(author.clone()).or_default().push(*sent_at as u64);
        }
        if send_receipts {
            for (author, timestamps) in by_author {
                if !self.is_accepted(&ConversationId::direct(&author)).await {
                    continue;
                }
                let receipt = ReceiptMessage {
                    r#type: Some(receipt_message::Type::Read.into()),
                    timestamp: timestamps,
                };
                self.enqueue(Outbound::Send {
                    conversation: ConversationId::direct(&author),
                    body: receipt.into(),
                    timestamp: self.next_timestamp() as u64,
                    message: None,
                });
            }
        }

        let config = crate::remote_config::current();
        let me = self.self_aci();
        let read = unread
            .into_iter()
            .chain(poll_votes.into_iter().map(|sent_at| (me.clone(), sent_at)))
            .map(|(author, sent_at)| {
                let sender = config.wire_service_id(&author);
                sync_message::Read {
                    sender_aci: sender.string(),
                    sender_aci_binary: sender.binary(),
                    timestamp: Some(sent_at as u64),
                }
            })
            .collect();
        self.enqueue(Outbound::Sync {
            body: SyncMessage { read, ..SyncMessage::with_padding(&mut rand::rng()) },
        });
    }

    async fn typing(self: Rc<Self>, conversation: ConversationId, started: bool) {
        if !self.setting_on(AccountSetting::TypingIndicators).await
            || conversation.as_direct() == Some(self.self_aci().as_str())
            // Not to a message request (Desktop's `shouldSendToConversation`).
            || !self.is_accepted(&conversation).await
        {
            return;
        }
        let timestamp = self.next_timestamp() as u64;
        let group_id = conversation.as_group().and_then(|key| {
            let key: [u8; 32] = key.try_into().ok()?;
            Some(GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key)).get_group_identifier().to_vec())
        });
        let action = if started { typing_message::Action::Started } else { typing_message::Action::Stopped };
        let typing = TypingMessage {
            timestamp: Some(timestamp),
            action: Some(action.into()),
            group_id,
        };
        self.enqueue(Outbound::Send { conversation, body: typing.into(), timestamp, message: None });
    }

    async fn image_optimization(&self) -> media::Optimization {
        let value = self.db.setting(crate::SETTING_IMAGE_OPTIMIZATION).await.ok().flatten();
        media::Optimization::from_setting(value.as_deref())
    }

    /// Whether an account setting is on (unset: Desktop's default).
    pub(crate) async fn setting_on(&self, setting: AccountSetting) -> bool {
        setting.is_on(self.db.setting(setting.key()).await.ok().flatten().as_deref())
    }

    /// Saves account settings from Storage Service, a configuration sync,
    /// the link-time backup or linking; tells the UI if one changed.
    pub(crate) async fn save_account_settings(&self, settings: crate::AccountSettings) {
        let flags = settings.values().map(|(setting, on)| (setting.key(), if on { "true" } else { "false" }.to_owned()));
        let timer = settings.universal_expire_timer.map(|t| (crate::SETTING_UNIVERSAL_EXPIRE_TIMER, t.to_string()));
        let sharing = settings
            .phone_number_sharing
            .map(|everybody| (crate::SETTING_PHONE_NUMBER_SHARING, everybody.to_string()));
        let mut changed = false;
        for (key, value) in flags.chain(timer).chain(sharing) {
            if self.db.setting(key).await.ok().flatten().as_deref() == Some(value.as_str()) {
                continue;
            }
            match self.db.set_setting(key, &value).await {
                Ok(()) => changed = true,
                Err(error) => warn!(%error, key, "failed to save an account setting"),
            }
        }
        if changed {
            self.emit(Event::AccountSettingsChanged);
        }
    }

    /// Desktop's Privacy toggles: here at once, then in the account record
    /// for our other devices (`captureChange`). If that write fails,
    /// re-reading Storage Service puts it back.
    async fn set_account_setting(self: Rc<Self>, setting: AccountSetting, on: bool) {
        self.save_account_settings(crate::AccountSettings::flag(setting, on)).await;
        self.write_account(&[storage::AccountEdit::Setting(setting, on)]).await;
    }

    async fn group_context(&self, conversation: &ConversationId) -> Option<GroupContextV2> {
        let key = conversation.as_group()?;
        let arr: [u8; 32] = key.clone().try_into().ok()?;
        let revision = self.store.group(arr).await.ok().flatten().map(|g| g.revision).unwrap_or(0);
        Some(GroupContextV2 { master_key: Some(key), revision: Some(revision), ..Default::default() })
    }

    fn enqueue(&self, job: Outbound) {
        let _ = self.outbound.unbounded_send(job);
    }

    async fn outbound_loop(self: Rc<Self>, mut jobs: mpsc::UnboundedReceiver<Outbound>) {
        while let Some(job) = jobs.next().await {
            let Some(mut manager) = self.manager() else {
                match job {
                    Outbound::Send { conversation, message: Some(id), timestamp, .. }
                    | Outbound::SendImages { conversation, message: id, timestamp, .. }
                    | Outbound::SendSticker { conversation, message: id, timestamp, .. }
                    | Outbound::SendGif { conversation, message: id, timestamp, .. } => {
                        self.finish_send(&conversation, id, timestamp, DeliveryStatus::Failed).await;
                    }
                    Outbound::Story { recipient, job, .. } => self.story_part_done(job, recipient, Err("not connected".into())),
                    Outbound::StoryBatch { recipients, job, .. } => {
                        for recipient in recipients {
                            self.story_part_done(job, recipient, Err("not connected".into()));
                        }
                    }
                    // Desktop's job queue waits until it can send.
                    Outbound::PollVote { message, vote_count, attempt } => {
                        self.send_poll_vote(None, message, vote_count, attempt).await;
                    }
                    _ => {}
                }
                continue;
            };
            // Nothing of ours goes to a message request until it's
            // accepted (Desktop's `shouldSendToConversation`).
            if let Some(chat) = job.message_to_any()
                && !self.is_accepted(chat).await
            {
                warn!("not sending to a message request");
                if let Some((conversation, id, timestamp)) = job.tracked() {
                    self.finish_send(&conversation, id, timestamp, DeliveryStatus::Failed).await;
                }
                continue;
            }
            if let Some(chat) = job.message_to() {
                self.share_profile_with(chat).await;
            }
            match job {
                Outbound::Send { conversation, body, timestamp, message } => {
                    let delivery = self.deliver_to(&mut manager, &conversation, body, timestamp, None).await;
                    let status = self.report(&conversation, &delivery);
                    if let Some(id) = message {
                        // Who our poll reached may vote in it.
                        self.poll_reached(&manager, id, &delivery.reached).await;
                        self.finish_send(&conversation, id, timestamp, status).await;
                    }
                }
                Outbound::SendImages { conversation, mut body, timestamp, message, images, preview_image } => {
                    let uploaded = match self.upload_images(&manager, images).await {
                        Ok(pointers) => match preview_image {
                            Some(path) => self.upload_preview_image(&manager, &path).await.map(|p| (pointers, Some(p))),
                            None => Ok((pointers, None)),
                        },
                        Err(error) => Err(error),
                    };
                    let status = match uploaded {
                        Ok((pointers, preview_pointer)) => {
                            body.attachments = pointers;
                            if let Some(preview) = body.preview.first_mut() {
                                preview.image = preview_pointer;
                            }
                            self.deliver(&mut manager, &conversation, body.into(), timestamp).await
                        }
                        Err(error) => {
                            self.error("failed to send images", format!("{error:#}"));
                            DeliveryStatus::Failed
                        }
                    };
                    self.finish_send(&conversation, message, timestamp, status).await;
                }
                Outbound::SendGif { conversation, mut body, timestamp, message, path, size } => {
                    let status = match self.upload_gif(&manager, &path, size).await {
                        Ok(pointer) => {
                            body.attachments = vec![pointer];
                            self.deliver(&mut manager, &conversation, body.into(), timestamp).await
                        }
                        Err(error) => {
                            self.error("failed to send GIF", format!("{error:#}"));
                            DeliveryStatus::Failed
                        }
                    };
                    self.finish_send(&conversation, message, timestamp, status).await;
                }
                Outbound::SendSticker { conversation, mut body, timestamp, message, path } => {
                    let status = match self.upload_preview_image(&manager, &path).await {
                        Ok(pointer) => {
                            if let Some(sticker) = &mut body.sticker {
                                sticker.data = Some(pointer);
                            }
                            self.deliver(&mut manager, &conversation, body.into(), timestamp).await
                        }
                        Err(error) => {
                            self.error("failed to send sticker", format!("{error:#}"));
                            DeliveryStatus::Failed
                        }
                    };
                    self.finish_send(&conversation, message, timestamp, status).await;
                }
                Outbound::Story { recipient, body, timestamp, job } => {
                    self.send_story_part(&mut manager, recipient, body, timestamp, job).await;
                }
                Outbound::StoryBatch { target, group, recipients, body, timestamp, job } => {
                    self.send_story_batch(&mut manager, &target, group, recipients, body, timestamp, job).await;
                }
                Outbound::PollVote { message, vote_count, attempt } => {
                    self.send_poll_vote(Some(&mut manager), message, vote_count, attempt).await;
                }
                Outbound::Sync { body } => {
                    let me = self.self_aci();
                    let Some(me) = ServiceId::parse_from_service_id_string(&me) else { continue };
                    if let Err(error) = manager.send_message(me, body, self.next_timestamp() as u64).await {
                        warn!(%error, "failed to send sync message");
                    }
                }
                Outbound::GroupUpdateTo { recipient, body, timestamp } => {
                    let Some(to) = ServiceId::parse_from_service_id_string(&recipient) else { continue };
                    if let Err(error) = manager.send_message(to, body, timestamp).await {
                        warn!(%error, "failed to send a group update to someone outside the group");
                    }
                }
            }
        }
    }

    async fn deliver(
        &self,
        manager: &mut SignalManager,
        conversation: &ConversationId,
        body: ContentBody,
        timestamp: u64,
    ) -> DeliveryStatus {
        let delivery = self.deliver_to(manager, conversation, body, timestamp, None).await;
        self.report(conversation, &delivery)
    }

    /// A send's status, showing why it failed: a group send fails when
    /// anyone but an unregistered member missed it, a 1:1 send when they did.
    fn report(&self, conversation: &ConversationId, delivery: &Delivery) -> DeliveryStatus {
        if let Some(error) = &delivery.error {
            self.error("failed to send", error);
            return DeliveryStatus::Failed;
        }
        if let Some((_, error)) = delivery.failed.first() {
            self.error("failed to send", error);
            return DeliveryStatus::Failed;
        }
        if conversation.as_group().is_none() && !delivery.unregistered.is_empty() {
            self.error("failed to send", "they're no longer on Signal");
            return DeliveryStatus::Failed;
        }
        DeliveryStatus::Sent
    }

    /// Sends to a chat (to `only` these of its recipients, when given) and
    /// says how it went for each, as Desktop keeps
    /// `sendStateByConversationId`. Nothing is shown here; see
    /// [`Actor::report`].
    async fn deliver_to(
        &self,
        manager: &mut SignalManager,
        conversation: &ConversationId,
        body: ContentBody,
        timestamp: u64,
        only: Option<&[String]>,
    ) -> Delivery {
        let failed = |error: String| Delivery { error: Some(error), ..Default::default() };
        // Its account is made up (as Desktop's): nothing goes to it.
        if conversation.is_signal() {
            warn!("not sending to the Signal chat");
            return failed("the Signal chat can't be sent to".into());
        }
        let mut body = self.stamp_timer(conversation, body).await;
        if let Err(error) = self.attach_long_text(manager, &mut body).await {
            return failed(format!("{error:#}"));
        }
        self.attach_quote_thumbnail(manager, &mut body).await;
        let mut delivery = Delivery::default();
        match (conversation.as_group(), mapping::service_id_of(conversation)) {
            (Some(key), _) => {
                let only: Option<Vec<ServiceId>> =
                    only.map(|list| list.iter().filter_map(|s| ServiceId::parse_from_service_id_string(s)).collect());
                match self.send_to_group(manager, conversation, &key, body, timestamp, only.as_deref()).await {
                    Ok(results) => {
                        for (recipient, result) in results {
                            delivery.record(recipient.service_id_string(), result.map_err(SendFailure::from));
                        }
                    }
                    Err(error) => {
                        let failure = SendFailure::from(error);
                        delivery.note(&failure);
                        delivery.error = Some(failure.message);
                    }
                }
            }
            (None, Some(recipient)) => {
                let id = recipient.service_id_string();
                if only.is_some_and(|only| !only.contains(&id)) {
                    return delivery;
                }
                let result = manager.send_message(recipient, body, timestamp).await;
                delivery.record(id, result.map_err(SendFailure::from));
            }
            (None, None) => return failed(format!("invalid conversation {conversation}")),
        }
        for (who, error) in &delivery.failed {
            warn!(recipient = %who, %error, "send failed");
        }
        delivery
    }

    /// A body over 2 KiB goes as Desktop sends it (`sendNormalMessage`,
    /// `ts/jobs/helpers/sendNormalMessage.preload.ts`): the whole text as a
    /// `text/x-signal-plain` attachment, first among the attachments, and
    /// only its first 2 KiB in the body. Mentions and formatting stay as
    /// they are, over the whole text. Edits go the same way.
    async fn attach_long_text(&self, manager: &SignalManager, body: &mut ContentBody) -> anyhow::Result<()> {
        use crate::long_text;
        let data = match body {
            ContentBody::DataMessage(data) => data,
            ContentBody::EditMessage(edit) => match edit.data_message.as_mut() {
                Some(data) => data,
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
        let Some(text) = data.body.as_deref().filter(|b| long_text::is_too_long(b)) else { return Ok(()) };
        if text.len() > long_text::MAX_TEXT_BYTES {
            anyhow::bail!("the message is over Signal's 64 KiB limit");
        }
        let bytes = text.as_bytes().to_vec();
        let trimmed = long_text::trim_body(text);
        let spec = AttachmentSpec {
            content_type: mapping::LONG_TEXT_TYPE.into(),
            length: bytes.len(),
            file_name: None,
            preview: None,
            voice_note: None,
            borderless: None,
            width: None,
            height: None,
            caption: None,
            blur_hash: None,
        };
        let pointer = manager
            .upload_attachment(spec, bytes)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("uploading the message's text: {e}"))?;
        data.attachments.insert(0, pointer);
        data.body = Some(trimmed);
        Ok(())
    }

    /// A quote of a photo goes with a thumbnail of it, as Desktop's do
    /// (`makeQuote`), so their client shows it beside the quote. Without one
    /// (the photo isn't here, or the upload failed) the quote still goes.
    async fn attach_quote_thumbnail(&self, manager: &SignalManager, body: &mut ContentBody) {
        let ContentBody::DataMessage(data) = body else { return };
        let Some(quote) = data.quote.as_mut() else { return };
        // Either field: which one we filled follows `binaryServiceId`.
        let (Some(sent_at), Some(author)) = (quote.id, quote.parse_author_aci().map(|a| a.service_id_string())) else {
            return;
        };
        let Some(first) = quote.attachments.first_mut() else { return };
        if first.thumbnail.is_some() || !first.content_type.as_deref().is_some_and(|t| t.starts_with("image/")) {
            return;
        }
        let author = (author != self.self_aci()).then_some(author);
        let Ok(Some(quoted)) = self.db.find_message_any(author.as_deref(), sent_at as i64).await else { return };
        let Some(path) =
            quoted.attachments.iter().find(|a| a.content_type.starts_with("image/")).and_then(|a| a.local_path.clone())
        else {
            return;
        };
        let uploaded = async {
            let bytes = tokio::fs::read(&path).await?;
            let png = tokio::task::spawn_blocking(move || media::quote_thumbnail(&bytes)).await??;
            let (_, width, height) = media::probe(&png).context("the thumbnail isn't an image")?;
            let spec = AttachmentSpec {
                content_type: "image/png".into(),
                length: png.len(),
                file_name: None,
                preview: None,
                voice_note: None,
                borderless: None,
                width: Some(width),
                height: Some(height),
                caption: None,
                blur_hash: None,
            };
            manager
                .upload_attachment(spec, png)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .map_err(|e| anyhow::anyhow!("uploading quote thumbnail: {e}"))
        };
        match uploaded.await {
            Ok(pointer) => first.thumbnail = Some(pointer),
            Err(error) => warn!(error = %format!("{error:#}"), "quote sent without its thumbnail"),
        }
    }

    /// Every message we send carries the chat's timer, and in 1:1 chats its
    /// version, as Desktop's do; explicit timer updates carry their own.
    async fn stamp_timer(&self, conversation: &ConversationId, mut body: ContentBody) -> ContentBody {
        let data = match &mut body {
            ContentBody::DataMessage(data) => Some(data),
            ContentBody::EditMessage(edit) => edit.data_message.as_mut(),
            _ => None,
        };
        if let Some(data) = data
            && data.flags.unwrap_or(0) & data_message::Flags::ExpirationTimerUpdate as u32 == 0
            && let Ok(Some(chat)) = self.db.conversation(conversation).await
        {
            data.expire_timer = (chat.expire_timer > 0).then_some(chat.expire_timer);
            if conversation.as_group().is_none() {
                data.expire_timer_version = Some(chat.expire_timer_version);
            }
        }
        body
    }

    /// Optimizes each image losslessly (replacing the local copy, which looks
    /// identical), then uploads them all. Other files go up as they are.
    async fn upload_images(
        &self,
        manager: &SignalManager,
        images: Vec<Upload>,
    ) -> anyhow::Result<Vec<AttachmentPointer>> {
        let mut uploads = Vec::with_capacity(images.len());
        for Upload { path, file_name, file_type, voice } in images {
            let original = tokio::fs::read(&path).await.with_context(|| format!("reading {}", path.display()))?;
            if let Some(content_type) = file_type {
                let spec = AttachmentSpec {
                    content_type,
                    length: original.len(),
                    file_name,
                    preview: None,
                    voice_note: voice.then_some(true),
                    borderless: None,
                    width: None,
                    height: None,
                    caption: None,
                    blur_hash: None,
                };
                uploads.push((spec, original));
                continue;
            }
            let before = original.len();
            let level = self.image_optimization().await;
            let prepared = tokio::task::spawn_blocking(move || media::prepare(&original, level)).await??;
            info!(before, after = prepared.bytes.len(), kind = prepared.content_type, "optimized image");
            tokio::fs::write(&path, &prepared.bytes).await?;
            let spec = AttachmentSpec {
                content_type: prepared.content_type.into(),
                length: prepared.bytes.len(),
                file_name,
                preview: None,
                voice_note: None,
                borderless: None,
                width: Some(prepared.width),
                height: Some(prepared.height),
                caption: None,
                blur_hash: None,
            };
            uploads.push((spec, prepared.bytes));
        }
        manager
            .upload_attachments(uploads)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .map(|r| r.map_err(|e| anyhow::anyhow!("uploading: {e}")))
            .collect()
    }

    /// The preview image was optimized when it was fetched; upload it as is.
    async fn upload_preview_image(&self, manager: &SignalManager, path: &std::path::Path) -> anyhow::Result<AttachmentPointer> {
        self.upload_preview_bytes(manager, tokio::fs::read(path).await?).await
    }

    /// A link preview's image, already optimized, uploaded as is.
    async fn upload_preview_bytes(&self, manager: &SignalManager, bytes: Vec<u8>) -> anyhow::Result<AttachmentPointer> {
        let (kind, width, height) = media::probe(&bytes).context("link preview image is not an image")?;
        let spec = AttachmentSpec {
            content_type: kind.content_type().into(),
            length: bytes.len(),
            file_name: None,
            preview: None,
            voice_note: None,
            borderless: None,
            width: Some(width),
            height: Some(height),
            caption: None,
            blur_hash: None,
        };
        manager
            .upload_attachment(spec, bytes)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("uploading link preview image: {e}"))
    }

    async fn finish_send(&self, conversation: &ConversationId, id: i64, timestamp: u64, status: DeliveryStatus) {
        if let Err(error) = self.db.update_status(conversation, timestamp as i64, status).await {
            self.error("failed to update message status", error);
        }
        // Our view-once message is erased once sent (Desktop's
        // `sendNormalMessage`); one that failed keeps its media to retry.
        if status == DeliveryStatus::Sent
            && let Ok(Some(message)) = self.db.message(id).await
            && message.view_once == Some(signal_store::ViewOnce::Ready)
        {
            self.erase_view_once(id, signal_store::ViewOnce::Gone).await;
        }
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
    }

    // --------------------------------------------------------- safety numbers

    /// Our ACI and identity key, and theirs, for a 1:1 chat.
    async fn identities(&self, conversation: &ConversationId) -> anyhow::Result<(Aci, IdentityKey, Aci, IdentityKey)> {
        let theirs = conversation
            .as_direct()
            .and_then(Aci::parse_from_service_id_string)
            .context("safety numbers are for chats with one person")?;
        let ours = Aci::parse_from_service_id_string(&self.self_aci()).context("not linked")?;
        let store = presage::store::Store::aci_protocol_store(&self.store);
        let our_key = *store.get_identity_key_pair().await.map_err(|e| anyhow::anyhow!("{e}"))?.identity_key();
        let address = ProtocolAddress::new(theirs.service_id_string(), DeviceId::new(1).map_err(|e| anyhow::anyhow!("{e}"))?);
        let their_key = store
            .get_identity(&address)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .context("there's no safety number until you've exchanged a message with them on this device")?;
        Ok((ours, our_key, theirs, their_key))
    }

    /// Desktop's "Your safety number with … has changed": when someone's
    /// identity key differs from the one last seen, a notice goes in their
    /// 1:1 chat and every group we share. The first key seen is just noted.
    async fn check_identity(&self, sid: &str) {
        let Some(aci) = Aci::parse_from_service_id_string(sid) else { return };
        let store = presage::store::Store::aci_protocol_store(&self.store);
        let Ok(device) = DeviceId::new(1) else { return };
        let Ok(Some(key)) = store.get_identity(&ProtocolAddress::new(aci.service_id_string(), device)).await else { return };
        let key = hex::encode(key.serialize());
        if self.identities_seen.borrow().get(sid) == Some(&key) {
            return;
        }
        let setting = format!("{SETTING_IDENTITY}{sid}");
        // A read that failed isn't "no key": compare again next message.
        let Ok(previous) = self.db.setting(&setting).await else { return };
        if previous.as_deref() == Some(key.as_str()) {
            self.identities_seen.borrow_mut().insert(sid.to_owned(), key);
            return;
        }
        if previous.as_deref().is_some_and(|p| !p.is_empty()) && !self.add_safety_notices(sid).await {
            // Not recorded: the next message tries again.
            return;
        }
        if let Err(error) = self.db.set_setting(&setting, &key).await {
            return self.error("failed to record an identity key", error);
        }
        self.identities_seen.borrow_mut().insert(sid.to_owned(), key);
    }

    /// The safety-number-changed notice in their 1:1 chat and the groups we
    /// share; `false` if one couldn't be saved.
    async fn add_safety_notices(&self, sid: &str) -> bool {
        let notice = signal_store::Notice::SafetyNumberChanged { who: sid.to_owned() };
        let at = now_millis();
        let direct = ConversationId::direct(sid);
        let mut chats = Vec::new();
        if self.db.conversation(&direct).await.ok().flatten().is_some() {
            chats.push(direct);
        }
        let mut all = self.db.conversations().await.unwrap_or_default();
        all.extend(self.db.archived_conversations().await.unwrap_or_default());
        chats.extend(all.into_iter().filter(|c| c.kind == ConversationKind::Group && c.members.iter().any(|m| m == sid)).map(|c| c.id));
        let mut saved = true;
        for chat in chats {
            // Notices dedupe by time: another one in this millisecond (a
            // timer change in the same message, say) would swallow this one.
            for offset in 0..10 {
                match self.db.insert_notice(&chat, at + offset, &notice).await {
                    Ok(Some(id)) => {
                        self.emit(Event::MessageChanged { conversation: chat.clone(), message_id: id });
                        break;
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        self.error("failed to add notice", error);
                        saved = false;
                        break;
                    }
                }
            }
        }
        self.emit(Event::ConversationsChanged);
        saved
    }

    async fn safety_number(&self, conversation: &ConversationId) -> anyhow::Result<crate::SafetyNumber> {
        let (ours, our_key, theirs, their_key) = self.identities(conversation).await?;
        let (digits, qr) = crate::safety::compute((ours, &our_key), (theirs, &their_key))?;
        let verified = self
            .db
            .setting(&format!("{SETTING_VERIFIED}{}", theirs.service_id_string()))
            .await?
            .is_some_and(|key| key == hex::encode(their_key.serialize()));
        let auto_verify = self.auto_verify(conversation).await;
        Ok(crate::SafetyNumber { digits, qr, verified, auto_verify })
    }

    async fn emit_safety_number(&self, conversation: ConversationId) {
        let result = self.safety_number(&conversation).await.map_err(|e| format!("{e:#}"));
        self.emit(Event::SafetyNumber { conversation, result });
    }

    /// Marks their current identity key verified (or clears it) and tells
    /// our other devices, as Desktop's "Mark as verified" does.
    async fn set_verified(&self, conversation: ConversationId, verified: bool) {
        let (theirs, their_key) = match self.identities(&conversation).await {
            Ok((_, _, theirs, key)) => (theirs, key),
            Err(error) => return self.error("can't verify", format!("{error:#}")),
        };
        let key = their_key.serialize();
        let value = if verified { hex::encode(&key) } else { String::new() };
        if let Err(error) = self.db.set_setting(&format!("{SETTING_VERIFIED}{}", theirs.service_id_string()), &value).await {
            return self.error("can't verify", error);
        }
        let state = if verified { presage::proto::verified::State::Verified } else { presage::proto::verified::State::Default };
        // Padding, as Desktop sends, so the message's size says nothing.
        let null_message: Vec<u8> = (0..rand::Rng::random_range(&mut rand::rng(), 1..=512)).map(|_| rand::random()).collect();
        let destination = crate::remote_config::current().wire_service_id(&theirs.service_id_string());
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::Verified(presage::proto::Verified {
                    destination_aci: destination.string(),
                    destination_aci_binary: destination.binary(),
                    identity_key: Some(key.to_vec()),
                    state: Some(state.into()),
                    null_message: Some(null_message),
                })),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
        self.emit_safety_number(conversation).await;
    }

    // ------------------------------------------------------------ attachments

    async fn fetch_attachment(self: Rc<Self>, message_id: i64, index: usize) {
        let Some(manager) = self.manager() else { return };
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return };
        // From transferred history: no stored message, but its locator.
        if let Some(meta) = message.attachments.get(index).filter(|a| a.remote.is_some()) {
            if let Err(error) = self.download_remote(&manager, &message.conversation_id, message_id, index, meta).await {
                self.error("couldn't download the attachment", format!("{error:#}"));
                // Desktop falls back to the backup thumbnail.
                if let Err(error) = self.fetch_thumbnail(&manager, &message, index, meta).await {
                    debug!(error = %format!("{error:#}"), "backup thumbnail download failed");
                }
            }
            return;
        }
        let thread = match message.conversation_id.as_group() {
            Some(key) => match key.try_into() {
                Ok(key) => Thread::Group(key),
                Err(_) => return,
            },
            None => match mapping::service_id_of(&message.conversation_id) {
                Some(sid) => Thread::Contact(sid),
                None => return,
            },
        };
        let content = match manager.store().message(&thread, message.sent_at as u64).await {
            Ok(Some(content)) => content,
            Ok(None) => return self.error("attachment unavailable", "original message not found"),
            Err(error) => return self.error("attachment unavailable", error),
        };
        let pointer = data_message_of(&content.body).and_then(|dm| mapping::normal_attachments(dm).nth(index).cloned());
        let Some(pointer) = pointer else {
            return self.error("attachment unavailable", "no such attachment");
        };
        self.download(&manager, message.conversation_id, message_id, index, &pointer).await;
    }

    /// Notes that a message's whole text came as an attachment, and
    /// downloads it.
    async fn expect_long_text(
        self: &Rc<Self>,
        manager: &SignalManager,
        message_id: i64,
        pointer: &AttachmentPointer,
        mentions: Vec<String>,
        styles: Vec<mapping::WireStyle>,
        held: bool,
    ) {
        let Some(remote) = mapping::remote_of(pointer) else {
            return warn!("a long text came without where to download it");
        };
        // Held for a message request: left as one not downloaded yet (it
        // can be asked for), and fetched once the request is accepted.
        let state = if held { signal_store::LongTextState::Failed } else { signal_store::LongTextState::Downloading };
        let long_text = signal_store::LongText {
            remote,
            size: pointer.size.map_or(0, u64::from),
            mentions,
            styles,
            state,
            attempts: 0,
            retry_at: None,
        };
        if let Err(error) = self.db.set_long_text(message_id, Some(&long_text)).await {
            return self.error("failed to store message", error);
        }
        if held {
            return;
        }
        let this = self.clone();
        let manager = manager.clone();
        tokio::task::spawn_local(async move { this.download_long_text(&manager, message_id, LongTextTry::First).await });
    }

    /// Downloads a message's whole text and puts it in place of the body's
    /// start, with its mentions and formatting (Desktop's
    /// `addAttachmentToMessage` for a `long-message`,
    /// `ts/messageModifiers/AttachmentDownloads.preload.ts`). One over
    /// Desktop's size limit isn't downloaded. A failed download is tried
    /// again on its own a few times, as Desktop's download job does
    /// (`crate::long_text::retry_delay_ms`), unless it's gone from the
    /// server; until then it can be asked for ("Download Full Message").
    async fn download_long_text(self: &Rc<Self>, manager: &SignalManager, message_id: i64, attempt: LongTextTry) {
        use signal_store::LongTextState;
        let Ok(Some(message)) = self.db.message(message_id).await else { return };
        let Some(long_text) = message.long_text else { return };
        // A retry that was asked for (or rescheduled) since has nothing to do.
        if let LongTextTry::Scheduled(at) = attempt
            && long_text.retry_at != Some(at)
        {
            return;
        }
        let conversation = message.conversation_id;
        let set_state = async |state: LongTextState, attempts: u32, retry_at: Option<i64>| {
            match self.db.set_long_text_state(message_id, &long_text, state, attempts, retry_at).await {
                Ok(true) => self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id }),
                Ok(false) => {}
                Err(error) => self.error("failed to store message", error),
            }
        };
        if long_text.size > crate::long_text::max_receive_bytes() {
            info!(size = long_text.size, "not downloading a long text over the limit");
            return set_state(LongTextState::TooBig, 0, None).await;
        }
        // Asking starts the count again, as a new Desktop job does.
        let attempts = if attempt == LongTextTry::Asked { 0 } else { long_text.attempts };
        if long_text.state != LongTextState::Downloading || long_text.retry_at.is_some() || attempts != long_text.attempts {
            set_state(LongTextState::Downloading, attempts, None).await;
        }
        let bytes = match self.remote_bytes(manager, &long_text.remote, long_text.size).await {
            Ok(bytes) => bytes,
            Err(error) => {
                let failures = attempts + 1;
                let gone = error.downcast_ref::<crate::history::Gone>().is_some();
                let retry_at = if gone { None } else { crate::long_text::retry_delay_ms(failures).map(|delay| now_millis() + delay) };
                warn!(error = %format!("{error:#}"), failures, retry_at, "couldn't download a long text");
                set_state(LongTextState::Failed, failures, retry_at).await;
                if let Some(at) = retry_at {
                    self.retry_long_text_at(manager, message_id, at);
                }
                return;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let (body, spans, style_spans) = self.render_body(&text, &long_text.mentions, &long_text.styles).await;
        match self.db.apply_long_text(message_id, &long_text, &body, &spans, &style_spans).await {
            Ok(true) => {
                self.emit(Event::MessageChanged { conversation, message_id });
                self.emit(Event::ConversationsChanged);
            }
            // Deleted or edited since.
            Ok(false) => {}
            Err(error) => self.error("failed to store message", error),
        }
    }

    /// Tries a long text again at `at` (unix millis).
    fn retry_long_text_at(self: &Rc<Self>, manager: &SignalManager, message_id: i64, at: i64) {
        // Only the newest time counts: a retry scheduled later replaces an
        // older one still waiting, which then wakes to nothing.
        if self.long_text_retries.borrow_mut().insert(message_id, at) == Some(at) {
            return;
        }
        let (this, manager) = (self.clone(), manager.clone());
        tokio::task::spawn_local(async move {
            let wait = u64::try_from(at - now_millis()).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(wait)).await;
            let current = {
                let mut retries = this.long_text_retries.borrow_mut();
                let current = retries.get(&message_id) == Some(&at);
                if current {
                    retries.remove(&message_id);
                }
                current
            };
            if current {
                this.download_long_text(&manager, message_id, LongTextTry::Scheduled(at)).await;
            }
        });
    }

    /// Long texts whose download was cut off (the app quit, say), tried
    /// again, a few at a time, once connected; and ones waiting to be
    /// tried again, at their time.
    async fn resume_long_texts(self: Rc<Self>, manager: SignalManager) {
        const PER_RUN: usize = 50;
        let ids = match self.db.long_texts_to_resume(PER_RUN).await {
            Ok(ids) => ids,
            Err(error) => return self.error("failed to read messages", error),
        };
        if !ids.is_empty() {
            info!(count = ids.len(), "downloading long texts again");
        }
        for (id, retry_at) in ids {
            match retry_at {
                Some(at) => self.retry_long_text_at(&manager, id, at),
                None => self.download_long_text(&manager, id, LongTextTry::First).await,
            }
        }
    }

    async fn download(
        &self,
        manager: &SignalManager,
        conversation: ConversationId,
        message_id: i64,
        index: usize,
        pointer: &AttachmentPointer,
    ) {
        // Too big to receive: left undownloaded, as Desktop leaves it.
        let size = pointer.size.map_or(0, u64::from);
        if size > crate::remote_config().attachment_max_receive_bytes() {
            return info!(size, "not downloading an attachment over the receive limit");
        }
        let bytes = match manager.get_attachment(pointer).await {
            Ok(bytes) => bytes,
            Err(error) => return self.error("failed to download attachment", error),
        };
        let ext = extension_for(pointer);
        let path = self
            .config
            .data_dir
            .join("attachments")
            .join(format!("{message_id}-{index}.{ext}"));
        if let Err(error) = tokio::fs::write(&path, &bytes).await {
            return self.error("failed to save attachment", error);
        }
        let path = path.to_string_lossy().into_owned();
        match self.db.set_attachment_path(message_id, index, &path).await {
            Ok(true) => self.emit(Event::MessageChanged { conversation, message_id }),
            // Erased (a view-once message viewed elsewhere) or deleted
            // while it downloaded: the file isn't kept.
            Ok(false) => {
                let _ = tokio::fs::remove_file(&path).await;
            }
            Err(error) => self.error("failed to record attachment", error),
        }
    }
}

impl Actor {
    /// A transferred attachment's file, from the backup (media) tier when
    /// there's one to read, else (or when that fails) the transit tier, as
    /// Desktop's `downloadAttachment` tries them
    /// (`ts/util/downloadAttachment.preload.ts`). A failure gives every
    /// tier's reason.
    async fn remote_bytes(&self, manager: &SignalManager, remote: &RemoteAttachment, size: u64) -> anyhow::Result<Vec<u8>> {
        let mut reasons = Vec::new();
        if crate::backup_media::media_parts(remote).is_some()
            && let Some(root) = crate::backup_media::BackupMedia::root_key(&self.db).await
        {
            match self.backup_media.download(manager, &self.db, &root, remote, size).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    debug!(error = %format!("{error:#}"), "backup tier download failed");
                    reasons.push(format!("from your backup: {error:#}"));
                }
            }
        }
        if remote.cdn_key.is_some() {
            match crate::history::download_attachment(manager, remote, size).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => reasons.push(format!("{error:#}")),
            }
        } else {
            reasons.push("it's only in your backup, which this device can't read".to_owned());
        }
        anyhow::bail!("{}", reasons.join("; "))
    }

    /// A transferred image's or video's thumbnail from the backup tier,
    /// shown until the file itself is downloaded (Desktop's
    /// `downloadBackupThumbnail`). `Ok(false)` when there's none to fetch.
    /// A view-once message's photo isn't previewed.
    async fn fetch_thumbnail(&self, manager: &SignalManager, message: &Message, index: usize, meta: &AttachmentMeta) -> anyhow::Result<bool> {
        let (conversation, message_id) = (&message.conversation_id, message.id);
        let Some(remote) = meta.remote.as_ref() else { return Ok(false) };
        if message.view_once.is_some()
            || meta.thumbnail_path.is_some()
            || !crate::backup_media::can_have_thumbnail(&meta.content_type)
            || crate::backup_media::media_parts(remote).is_none()
        {
            return Ok(false);
        }
        let Some(root) = crate::backup_media::BackupMedia::root_key(&self.db).await else { return Ok(false) };
        let bytes = self.backup_media.download_thumbnail(manager, &self.db, &root, remote).await?;
        let path = self.config.data_dir.join("attachments").join(format!("{message_id}-{index}-thumbnail.webp"));
        tokio::fs::write(&path, &bytes).await?;
        if !self.db.set_attachment_thumbnail(message_id, index, &path.to_string_lossy()).await? {
            // Erased or deleted while it downloaded.
            let _ = tokio::fs::remove_file(&path).await;
            return Ok(false);
        }
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id });
        Ok(true)
    }

    /// Keeps the media root backup key (or forgets it, when a key sync
    /// comes without one, as Desktop's `onKeysSync` does). A new key means
    /// a new signing key to upload and new credentials.
    async fn set_media_root_key(&self, key: Option<[u8; 32]>) {
        use crate::backup_media::{SETTING_MEDIA_ROOT_KEY, SETTING_SIGNATURE_KEY_SET};
        let value = key.map(hex::encode).unwrap_or_default();
        let old = self.db.setting(SETTING_MEDIA_ROOT_KEY).await.ok().flatten().unwrap_or_default();
        if old == value {
            return;
        }
        info!(present = key.is_some(), "media root backup key changed");
        if let Err(error) = self.db.set_setting(SETTING_MEDIA_ROOT_KEY, &value).await {
            return self.error("failed to save the media backup key", error);
        }
        let _ = self.db.set_setting(SETTING_SIGNATURE_KEY_SET, "").await;
        self.backup_media.reset().await;
    }

    /// The account's backup tier from its storage record (Desktop's
    /// `saveBackupTier`, which resets its cached backup data on a change).
    async fn save_backup_tier(&self, tier: u64) {
        use crate::backup_media::SETTING_BACKUP_TIER;
        let tier = tier.to_string();
        let old = self.db.setting(SETTING_BACKUP_TIER).await.ok().flatten();
        if old.as_deref() == Some(tier.as_str()) {
            return;
        }
        if let Err(error) = self.db.set_setting(SETTING_BACKUP_TIER, &tier).await {
            return warn!(%error, "failed to save the backup tier");
        }
        info!(%tier, "backup tier changed");
        self.backup_media.reset().await;
    }

    /// Asks the phone for its keys when the media root key is missing (a
    /// device linked before it was kept), at most daily, as Desktop's
    /// `ensureAEP` asks for a missing account entropy pool.
    async fn request_keys_if_missing(&self) {
        use crate::backup_media::{SETTING_KEYS_REQUESTED, SETTING_MEDIA_ROOT_KEY};
        if self.db.setting(SETTING_MEDIA_ROOT_KEY).await.ok().flatten().is_some_and(|k| !k.is_empty()) {
            return;
        }
        let now = now_millis();
        let last = self.db.setting(SETTING_KEYS_REQUESTED).await.ok().flatten().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        if now - last < 24 * 60 * 60 * 1000 && last <= now {
            return;
        }
        info!("no media backup key; asking the phone for its keys");
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::Request(sync_message::Request {
                    r#type: Some(sync_message::request::Type::Keys as i32),
                })),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
        let _ = self.db.set_setting(SETTING_KEYS_REQUESTED, &now.to_string()).await;
    }

    /// Downloads an attachment from its saved locator.
    async fn download_remote(
        &self,
        manager: &SignalManager,
        conversation: &ConversationId,
        message_id: i64,
        index: usize,
        meta: &AttachmentMeta,
    ) -> anyhow::Result<()> {
        let remote = meta.remote.as_ref().context("no locator")?;
        if meta.size > crate::remote_config().attachment_max_receive_bytes() {
            anyhow::bail!("it's over Signal's receive limit");
        }
        let bytes = self.remote_bytes(manager, remote, meta.size).await?;
        let pointer = AttachmentPointer {
            content_type: Some(meta.content_type.clone()),
            file_name: meta.file_name.clone(),
            ..Default::default()
        };
        let path = self
            .config
            .data_dir
            .join("attachments")
            .join(format!("{message_id}-{index}.{}", extension_for(&pointer)));
        tokio::fs::write(&path, &bytes).await?;
        if !self.db.set_attachment_path(message_id, index, &path.to_string_lossy()).await? {
            // Erased or deleted while it downloaded.
            let _ = tokio::fs::remove_file(&path).await;
            return Ok(());
        }
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id });
        Ok(())
    }

    /// The sticker packs installed on the phone, from the transfer: each
    /// downloaded unless it's already here.
    async fn upload_sticker_pack(&self, pack: crate::sticker_upload::NewStickerPack) {
        use crate::sticker_upload::{StickerUpload, share_url, upload};
        let Some(mut manager) = self.manager() else {
            self.emit(Event::StickerUpload(StickerUpload::Failed("not linked".into())));
            return;
        };
        let progress = |done, total| self.emit(Event::StickerUpload(StickerUpload::Progress { done, total }));
        match upload(&mut manager, &pack, progress).await {
            Ok((pack_id, pack_key)) => {
                info!(stickers = pack.stickers.len(), "uploaded a sticker pack");
                let url = share_url(&pack_id, &pack_key);
                self.emit(Event::StickerUpload(StickerUpload::Done { pack_id, pack_key, url }));
                let packs = self.load_stickers().await;
                self.emit(Event::Stickers(packs));
            }
            Err(error) => {
                warn!(error = format!("{error:#}"), "couldn't upload a sticker pack");
                self.emit(Event::StickerUpload(StickerUpload::Failed(format!("{error:#}"))));
            }
        }
    }

    async fn install_sticker_packs(self: Rc<Self>, mut manager: SignalManager, packs: Vec<(Vec<u8>, Vec<u8>)>) {
        let installed: Vec<Vec<u8>> = match self.store.sticker_packs().await {
            Ok(packs) => packs.flatten().map(|p| p.id).collect(),
            Err(_) => Vec::new(),
        };
        let mut added = 0;
        for (id, key) in packs {
            if installed.contains(&id) {
                continue;
            }
            match manager.install_sticker_pack(&id, &key).await {
                Ok(()) => added += 1,
                Err(error) => warn!(%error, "couldn't install a transferred sticker pack"),
            }
        }
        info!(added, "installed transferred sticker packs");
    }

    /// Attachments from transferred history, one at a time: photos and GIFs
    /// whole, falling back to their backup thumbnail as Desktop's download
    /// job does, and other images and videos as thumbnails only (what
    /// Desktop shows first for messages on screen). Ones that fail (expired
    /// from the transit tier, usually) stay downloadable by hand, which
    /// says why.
    async fn download_transferred(self: Rc<Self>, manager: SignalManager, downloads: Vec<(i64, usize)>) {
        let total = downloads.len();
        let (mut whole, mut thumbnails, mut failed) = (0, 0, 0);
        for (message_id, index) in downloads {
            let Some(message) = self.db.message(message_id).await.ok().flatten() else { continue };
            let Some(meta) = message.attachments.get(index).filter(|a| a.local_path.is_none()) else { continue };
            let conversation = &message.conversation_id;
            if downloads_whole(meta) {
                match self.download_remote(&manager, conversation, message_id, index, meta).await {
                    Ok(()) => {
                        whole += 1;
                        continue;
                    }
                    Err(error) => debug!(error = %format!("{error:#}"), "transferred attachment download failed"),
                }
            }
            match self.fetch_thumbnail(&manager, &message, index, meta).await {
                Ok(true) => thumbnails += 1,
                Ok(false) => failed += usize::from(downloads_whole(meta)),
                Err(error) => {
                    debug!(error = %format!("{error:#}"), "backup thumbnail download failed");
                    failed += 1;
                }
            }
        }
        info!(total, whole, thumbnails, failed, "downloaded transferred attachments");
    }

    async fn download_preview_image(
        &self,
        manager: &SignalManager,
        conversation: ConversationId,
        message_id: i64,
        pointer: &AttachmentPointer,
    ) {
        let bytes = match manager.get_attachment(pointer).await {
            Ok(bytes) => bytes,
            Err(error) => return debug!(%error, "failed to download link preview image"),
        };
        let path = self
            .config
            .data_dir
            .join("attachments")
            .join(format!("{message_id}-preview.{}", extension_for(pointer)));
        if let Err(error) = tokio::fs::write(&path, &bytes).await {
            return self.error("failed to save link preview image", error);
        }
        match self.db.set_preview_image_path(message_id, &path.to_string_lossy()).await {
            Ok(()) => self.emit(Event::MessageChanged { conversation, message_id }),
            Err(error) => self.error("failed to record link preview image", error),
        }
    }
}

/// What a quote of `message` says it holds: its first attachment, or for a
/// view-once message only that (Desktop's `makeQuote`, a JPEG without a
/// thumbnail).
fn quoted_attachment(message: &signal_store::Message) -> Option<signal_store::QuotedAttachment> {
    if message.view_once.is_some() {
        return Some(signal_store::QuotedAttachment::view_once());
    }
    message.attachments.first().map(signal_store::QuotedAttachment::of)
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn data_message_of(body: &ContentBody) -> Option<&DataMessage> {
    match body {
        ContentBody::DataMessage(dm) => Some(dm),
        ContentBody::EditMessage(edit) => edit.data_message.as_ref(),
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(sync_message::Content::Sent(sent)),
            ..
        }) => sent
            .message
            .as_ref()
            .or_else(|| sent.edit_message.as_ref().and_then(|e| e.data_message.as_ref())),
        _ => None,
    }
}

/// File extension for a downloaded attachment: from its file name, else its MIME type.
pub(crate) fn extension_for(pointer: &AttachmentPointer) -> String {
    let from_name = pointer
        .file_name
        .as_deref()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, ext)| ext)
        .filter(|ext| !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    if let Some(ext) = from_name {
        return ext.to_ascii_lowercase();
    }
    match pointer.content_type.as_deref().unwrap_or_default() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "image/avif" => "avif",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/aac" => "aac",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    }
    .to_owned()
}

fn short_id(service_id: &str) -> String {
    let id = service_id.strip_prefix("PNI:").unwrap_or(service_id);
    id.get(..8).unwrap_or(id).to_owned()
}

/// How often progress is reported while something long runs.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Rate-limits progress events: `ready` is true at most once per interval
/// (and on the first call).
struct Every {
    interval: Duration,
    last: Option<Instant>,
}

impl Every {
    fn new(interval: Duration) -> Self {
        Self { interval, last: None }
    }

    fn ready(&mut self) -> bool {
        let now = Instant::now();
        if self.last.is_some_and(|last| now.duration_since(last) < self.interval) {
            return false;
        }
        self.last = Some(now);
        true
    }
}
