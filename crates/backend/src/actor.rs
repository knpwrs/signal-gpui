//! The backend actor: owns the presage manager and applies incoming content to
//! the app database. Runs on a `LocalSet` because presage's futures are `!Send`.

mod calls;
mod group_calls;
mod release_notes;

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
    NewMessage, Quote, Reaction,
};
use tracing::{debug, info, warn};

use crate::{
    Command, Config, ConnectionState, Event, SETTING_LINK_PREVIEWS, SETTING_READ_RECEIPTS, SETTING_TYPING_INDICATORS,
    VOICE_CONTENT_TYPE, mapping::{self, Action, MapContext},
    media, storage,
};
use anyhow::Context as _;

pub(crate) type SignalManager = Manager<SqliteStore, Registered>;

/// Images up to this size are downloaded as soon as they arrive.
/// When this device was linked (unix millis).
const SETTING_DEVICE_CREATED: &str = "device.created_at";
/// "1" while the history transfer after linking hasn't finished.
const SETTING_HISTORY_PENDING: &str = "history.pending";
/// `avatar.checked.<service id>`: when we last fetched their profile for
/// its photo (unix millis).
const SETTING_AVATAR_CHECKED: &str = "avatar.checked.";
const AVATAR_CHECK_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
const AVATAR_CHECKS_PER_SYNC: usize = 100;
const AVATAR_BATCH_DELAY: Duration = Duration::from_secs(5 * 60);
/// Read syncs kept for messages not here yet.
const MAX_EARLY_READS: usize = 1000;
/// `verified.<ACI>`: the identity key (hex) we marked verified for them;
/// empty when cleared. Verification lapses when their key changes.
const SETTING_VERIFIED: &str = "verified.";
/// `identity.<ACI>`: their identity key (hex) as last seen, to notice a
/// change.
const SETTING_IDENTITY: &str = "identity.";
pub(crate) const AUTO_DOWNLOAD_MAX: u64 = 25 * 1024 * 1024;

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
    /// Sync message to our other devices.
    Sync { body: SyncMessage },
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
    /// Group identifier -> master key, for typing indicators.
    group_ids: RefCell<HashMap<Vec<u8>, [u8; 32]>>,
    profile_fetches: RefCell<HashSet<String>>,
    receiver: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Fetches release notes into the Signal chat.
    release_notes: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Shows megaphones; notified when they change.
    megaphones: RefCell<Option<tokio::task::JoinHandle<()>>>,
    megaphone_check: tokio::sync::Notify,
    /// Read syncs that arrived before their message, `(author, sent_at)`,
    /// applied when it does (as Desktop keeps them). Newest last.
    early_reads: RefCell<std::collections::VecDeque<(String, i64)>>,
    /// RingRTC, once a call needs it.
    calls: RefCell<Option<signal_calls::Calls>>,
    /// The call in progress.
    call: RefCell<Option<calls::ActiveCall>>,
    /// The group call we're in, or being rung for.
    group_call: RefCell<Option<group_calls::GroupCall>>,
    /// Lobby peeks awaiting their answer, by request id.
    peeks: RefCell<HashMap<u32, ConversationId>>,
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
    /// Wakes the disappearing-message sweeper when a countdown starts.
    expiry: tokio::sync::Notify,
    /// The last timestamp we sent with: each send gets a later one, since
    /// a timestamp names a message (two in one millisecond would collide).
    last_sent_at: std::cell::Cell<i64>,
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
        group_ids: RefCell::default(),
        profile_fetches: RefCell::default(),
        receiver: RefCell::new(None),
        release_notes: RefCell::new(None),
        megaphones: RefCell::new(None),
        megaphone_check: tokio::sync::Notify::new(),
        early_reads: RefCell::default(),
        identities_seen: RefCell::default(),
        calls: RefCell::new(None),
        call: RefCell::new(None),
        group_call: RefCell::new(None),
        peeks: RefCell::default(),
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
        expiry: tokio::sync::Notify::new(),
        last_sent_at: std::cell::Cell::new(0),
    });

    tokio::task::spawn_local(actor.clone().outbound_loop(outbound_rx));
    tokio::task::spawn_local(actor.clone().expiry_loop());
    tokio::task::spawn_local(actor.clone().call_loop(call_events_rx));
    tokio::task::spawn_local(actor.clone().call_send_loop(call_sends_rx));
    actor.load_call_choices().await;

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
            // Desktop's device age, for a donation megaphone's condition.
            let _ = self.db.set_setting(SETTING_DEVICE_CREATED, &now_millis().to_string()).await;
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
            let packs = std::mem::take(&mut imported.sticker_packs);
            if !packs.is_empty() {
                tokio::task::spawn_local(self.clone().install_sticker_packs(manager.clone(), packs));
            }
            let downloads = std::mem::take(&mut imported.downloads);
            if !downloads.is_empty() {
                tokio::task::spawn_local(self.clone().download_transferred(manager.clone(), downloads));
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
                Command::SendGif { conversation, gif } => self.send_gif(conversation, gif).await,
                Command::LoadStickers => {
                    let packs = self.load_stickers().await;
                    self.emit(Event::Stickers(packs));
                }
                Command::SendSticker { conversation, pack, sticker } => self.send_sticker(conversation, &pack, sticker).await,
                Command::UploadStickerPack(pack) => self.upload_sticker_pack(pack).await,
                Command::FindRecipient { query } => {
                    let result = self.find_recipient(&query).await;
                    self.emit(Event::RecipientFound { query, result });
                }
                Command::FetchLinkPreview { url } => {
                    let preview = if self.setting_enabled(SETTING_LINK_PREVIEWS).await {
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
                Command::DeleteForEveryone { message_id } => self.delete_for_everyone(message_id).await,
                Command::FetchAttachment { message_id, index } => {
                    self.fetch_attachment(message_id, index).await
                }
                Command::SetPinned { conversation, pinned } => self.set_pinned(conversation, pinned).await,
                Command::SetChat { conversation, change } => self.set_chat(conversation, change).await,
                Command::SetExpireTimer { conversation, seconds } => self.set_expire_timer(conversation, seconds).await,
                Command::UpdateGroup { conversation, title, description } => {
                    self.update_group(conversation, title, description).await
                }
                Command::LeaveGroup { conversation } => self.leave_group(conversation).await,
                Command::CreateGroup { title, members } => self.create_group(title, members).await,
                Command::SetMemberLabel { conversation, emoji, text } => self.set_member_label(conversation, emoji, text).await,
                Command::LoadSafetyNumber { conversation } => self.emit_safety_number(conversation).await,
                Command::SetVerified { conversation, verified } => self.set_verified(conversation, verified).await,
                Command::PressMegaphone { id, cta } => self.press_megaphone(id, cta).await,
                Command::StartCall { conversation, video } => self.start_call(conversation, video),
                Command::AcceptCall { video } => self.accept_call(video),
                Command::DeclineCall | Command::HangUp => self.hang_up(),
                Command::SetCallAudio(enabled) => self.set_call_audio(enabled),
                Command::SetCallVideo(enabled) => self.set_call_video(enabled),
                Command::LoadCallDevices => self.load_call_devices().await,
                Command::LoadShareSources => self.load_share_sources().await,
                Command::StartSharing { source } => self.start_sharing(source),
                Command::StopSharing => self.stop_sharing(),
                Command::SetCallDevice { kind, id } => self.set_call_device(kind, id).await,
                Command::PeekGroupCall { conversation } => self.peek_group_call(conversation).await,
                Command::Unlink => self.unlink().await,
                Command::SkipHistory => self.skip_history().await,
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
        for dir in ["avatars", "attachments", "stickers", "gif-cache"] {
            let _ = std::fs::remove_dir_all(self.config.data_dir.join(dir));
        }
        let _ = std::fs::create_dir_all(self.config.data_dir.join("attachments"));
        self.titled.borrow_mut().clear();
        self.names.borrow_mut().clear();
        self.self_aci.borrow_mut().clear();
        self.storage_synced.replace(false);
    }

    // -------------------------------------------------------------- receiving

    async fn receive_loop(self: Rc<Self>, mut manager: SignalManager) {
        let mut backoff = Duration::from_secs(1);
        let mut caught_up = false;
        let mut catch_up_first = None;
        let mut catch_up_every = Every::new(PROGRESS_INTERVAL);
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
                if !self.storage_synced.replace(true) {
                    self.spawn_storage_sync(manager);
                }
            }
            Received::Contacts => {
                info!("contacts synchronized");
                self.names.borrow_mut().clear();
                self.titled.borrow_mut().clear();
                self.refresh_titles().await;
                self.backfill_author_names().await;
            }
            Received::DecryptionError(sender) => {
                warn!(sender = %sender.service_id_string(), "failed to decrypt message");
            }
            Received::Content(content) => {
                let self_aci = self.self_aci();
                let sender = content.metadata.sender.service_id_string();
                if sender != self_aci {
                    self.check_identity(&sender).await;
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
                // Like Desktop, nothing from someone you blocked gets through.
                if sender != self_aci && self.is_blocked(&ConversationId::direct(&sender)).await {
                    return;
                }
                let ctx = MapContext { self_aci: &self_aci, now: now_millis() };
                for action in mapping::map_content(&content, &ctx) {
                    // Nor from a blocked group (our own sync messages still apply).
                    if let Action::Insert { message, .. } = &action
                        && message.direction == Direction::Incoming
                        && self.is_blocked(&message.conversation_id).await
                    {
                        continue;
                    }
                    self.apply(manager, action).await;
                }
            }
        }
    }

    async fn apply(self: &Rc<Self>, manager: &SignalManager, action: Action) {
        match action {
            Action::Insert { mut message, mentions, styles, pointers, preview_image, timer_version } => {
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
                    // Like Desktop, show the quoted message's photo or link
                    // preview image beside the quote when we have it.
                    if let Ok(Some(quoted)) = self.db.find_message_any(quote.author.as_deref(), quote.sent_at).await {
                        quote.thumbnail = quoted.thumbnail();
                        // Ours knows what the wire doesn't: a voice message.
                        if let Some(attachment) = quoted.attachments.first() {
                            quote.attachment = Some(signal_store::QuotedAttachment::of(attachment));
                        }
                    }
                }
                let conversation = message.conversation_id.clone();
                let downloadable: Vec<(usize, AttachmentPointer)> = pointers
                    .into_iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        // Photos, GIFs (looping videos) and voice messages
                        // download on their own (as in Desktop).
                        let flags = p.flags.unwrap_or(0);
                        let gif = flags & attachment_pointer::Flags::Gif as u32 != 0;
                        let voice = flags & attachment_pointer::Flags::VoiceMessage as u32 != 0;
                        (gif || voice || p.content_type.as_deref().is_some_and(|c| c.starts_with("image/")))
                            && p.size.is_some_and(|size| u64::from(size) <= AUTO_DOWNLOAD_MAX)
                    })
                    .collect();
                // Re-deliveries are de-duplicated by the store; only notify once.
                let repeat = self
                    .db
                    .find_message(&message.conversation_id, message.author.as_deref(), message.sent_at)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                match self.db.insert_message(&message).await {
                    Ok(id) => {
                        if !spans.is_empty() {
                            let _ = self.db.set_mentions(id, &spans).await;
                        }
                        if !style_spans.is_empty() {
                            let _ = self.db.set_styles(id, &style_spans).await;
                        }
                        // Already read on another device, before it got here.
                        let read_early = !repeat
                            && message.direction == Direction::Incoming
                            && match &message.author {
                                Some(author) => self.apply_early_read(author, message.sent_at).await,
                                None => false,
                            };
                        if !repeat && !read_early && message.direction == Direction::Incoming {
                            let me = self.self_aci();
                            self.emit(Event::Incoming {
                                conversation: conversation.clone(),
                                message_id: id,
                                mentions_me: spans.iter().any(|s| s.aci == me),
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
                        if let Some(pointer) = preview_image.filter(|p| {
                            p.content_type.as_deref().is_some_and(|c| c.starts_with("image/"))
                                && p.size.is_some_and(|size| u64::from(size) <= AUTO_DOWNLOAD_MAX)
                        }) {
                            let this = self.clone();
                            let manager = manager.clone();
                            let conversation = conversation.clone();
                            tokio::task::spawn_local(async move {
                                this.download_preview_image(&manager, conversation, id, &pointer).await
                            });
                        }
                        for (index, pointer) in downloadable {
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
            Action::React { conversation, target_author, target_sent_at, emoji, author, sent_at, remove } => {
                let Some(target) = self.find(&conversation, target_author.as_deref(), target_sent_at).await else {
                    debug!("reaction target not found");
                    return;
                };
                let result = if remove {
                    self.db.remove_reaction(target, author.as_deref()).await
                } else {
                    self.db.set_reaction(target, Reaction { emoji, author, sent_at }).await
                };
                match result {
                    Ok(()) => self.emit(Event::MessageChanged { conversation, message_id: target }),
                    Err(error) => self.error("failed to store reaction", error),
                }
            }
            Action::Delete { conversation, target_author, target_sent_at, sent_at } => {
                // Found by author too, so only the author's delete matches.
                let Some(message) = self.db.find_message(&conversation, target_author.as_deref(), target_sent_at).await.ok().flatten() else {
                    return;
                };
                if !crate::deletes::can_receive(&message, self.is_note_to_self(&conversation), sent_at) {
                    tracing::info!("ignoring a delete for everyone that came too late");
                    return;
                }
                let target = message.id;
                match self.db.mark_deleted(target).await {
                    Ok(()) => {
                        self.emit(Event::MessageChanged { conversation, message_id: target });
                        self.emit(Event::ConversationsChanged);
                    }
                    Err(error) => self.error("failed to delete message", error),
                }
            }
            Action::Edit { conversation, target_author, target_sent_at, sent_at, body, mentions, styles } => {
                let Some(target) = self.find(&conversation, target_author.as_deref(), target_sent_at).await else {
                    return;
                };
                let (body, spans, style_spans) = self.render_body(&body, &mentions, &styles).await;
                match self.db.edit_message(target, &body, sent_at).await {
                    Ok(()) => {
                        let _ = self.db.set_mentions(target, &spans).await;
                        let _ = self.db.set_styles(target, &style_spans).await;
                        // A read sync can name the edit, and come before it.
                        if let Some(author) = &target_author {
                            self.apply_early_read(author, sent_at).await;
                        }
                        self.emit(Event::MessageChanged { conversation, message_id: target });
                        self.emit(Event::ConversationsChanged);
                    }
                    Err(error) => self.error("failed to edit message", error),
                }
            }
            Action::Receipt { sent_at, status } => {
                for ts in sent_at {
                    match self.db.update_status_by_sent_at(ts, status).await {
                        Ok(changed) => {
                            for (conversation, message_id) in changed {
                                self.emit(Event::MessageChanged { conversation, message_id });
                            }
                        }
                        Err(error) => self.error("failed to apply receipt", error),
                    }
                }
            }
            Action::Typing { sender, group_id, started } => {
                let conversation = match group_id {
                    None => ConversationId::direct(&sender),
                    Some(id) => match self.group_for_id(manager, &id).await {
                        Some(key) => ConversationId::group(&key),
                        None => return,
                    },
                };
                self.emit(Event::Typing { conversation, author: sender, started });
            }
            Action::CallEvent(event) => self.on_call_event(event).await,
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
            Action::ReadSync(entries) => {
                let mut changed = false;
                for (author, sent_at) in entries {
                    match self.db.apply_read_sync(&author, sent_at).await {
                        Ok(signal_store::ReadSync::Read(_)) => changed = true,
                        Ok(signal_store::ReadSync::AlreadyRead) => {}
                        Ok(signal_store::ReadSync::NotFound) => {
                            let mut early = self.early_reads.borrow_mut();
                            if early.len() >= MAX_EARLY_READS {
                                early.pop_front();
                            }
                            early.push_back((author, sent_at));
                        }
                        Err(error) => self.error("failed to apply read sync", error),
                    }
                }
                if changed {
                    self.expiry.notify_one();
                    self.emit(Event::ConversationsChanged);
                }
            }
            // Before the first sync (which runs once the queue drains) there
            // is nothing to refresh.
            Action::DeleteForMe { messages, conversations } => {
                for (thread, author, sent_at) in messages {
                    let Some(conversation) = self.resolve_thread(manager, &thread).await else { continue };
                    let Some(id) = self.find(&conversation, author.as_deref(), sent_at).await else { continue };
                    self.remove_message(&conversation, id).await;
                }
                for chat in conversations {
                    if let Some(conversation) = self.resolve_thread(manager, &chat.thread).await {
                        self.delete_chat_from_sync(&conversation, &chat).await;
                    }
                }
            }
            Action::StorageChanged if *self.storage_synced.borrow() => self.spawn_storage_sync(manager),
            Action::StorageChanged => {}
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
        match self.db.pin_message(conversation, target, at, expires, crate::MAX_PINNED_MESSAGES).await {
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
    async fn create_group(self: Rc<Self>, title: String, members: Vec<String>) {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return self.emit(Event::Error("A group needs a name".into()));
        }
        let Some(manager) = self.manager() else { return };
        let Some(me) = Aci::parse_from_service_id_string(&self.self_aci()) else { return };
        let mine = manager.registration_data().profile_key();
        let mut candidates = Vec::new();
        for sid in &members {
            let Some(aci) = Aci::parse_from_service_id_string(sid) else { continue };
            let key = self.store.profile_key(&ServiceId::from(aci)).await.ok().flatten();
            candidates.push((aci, key));
        }
        let key: [u8; 32] = rand::Rng::random(&mut rand::rng());
        let created = async {
            let session = crate::groups::GroupSession::new(&manager, key).await?;
            session.create(&title, (me, mine), &candidates).await?;
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
    async fn leave_group(self: Rc<Self>, conversation: ConversationId) {
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
        let session = crate::groups::GroupSession::new(&manager, key).await?;
        let current = session.fetch().await?;
        let mut actions = presage::proto::group_change::Actions { version: current.version + 1, ..Default::default() };
        build(&session.operations, &mut actions)?;
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
        if !left && let Ok(group) = session.fetch().await
            && let Err(error) = self.store.save_group(key, group).await
        {
            warn!(%error, "failed to save the changed group");
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
        let summary = mapping::summarize_group_change(&changes, &self.self_aci());
        if let Some(seconds) = summary.timer {
            let version = self.db.conversation(&conversation).await.ok().flatten().map_or(1, |c| c.expire_timer_version);
            let _ = self.db.set_expire_timer(&conversation, seconds, version).await;
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
            if pins.len() >= storage::MAX_PINNED {
                return self.emit(Event::Error(format!("You can pin up to {} chats", storage::MAX_PINNED)));
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

    /// Reads names, profile keys, pins and archived/muted state from the
    /// Storage Service, then re-titles.
    async fn sync_storage(self: &Rc<Self>, manager: &SignalManager) {
        let storage::StorageSnapshot { contacts, pinned, chats, link_previews } = match storage::fetch(manager).await {
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
        if let Some(enabled) = link_previews
            && let Err(error) = self.db.set_setting(SETTING_LINK_PREVIEWS, if enabled { "true" } else { "false" }).await
        {
            warn!(%error, "failed to save link preview setting");
        }
        match self.db.apply_chat_states(&chats).await {
            Ok(true) => self.emit(Event::ConversationsChanged),
            Ok(false) => {}
            Err(error) => self.error("failed to save archived and muted chats", error),
        }
        let mut store = self.store.clone();
        let mut names = Vec::new();
        for contact in &contacts {
            let service_id = Aci::from(contact.aci).service_id_string();
            if let Some(key) = contact.profile_key
                && let Err(error) = store.upsert_profile_key(&contact.aci, ProfileKey::create(key)).await
            {
                debug!(%error, "failed to save profile key");
            }
            if let Some(name) = &contact.name {
                names.push((service_id, name.clone()));
            }
        }
        if let Err(error) = self.db.replace_contact_names(&names).await {
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
    /// and queues the upload.
    async fn send_gif(self: Rc<Self>, conversation: ConversationId, gif: crate::giphy::Gif) {
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
        };
        let Some((id, data, timestamp)) = self.store_outgoing(&conversation, String::new(), None, vec![meta], None).await
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

    async fn send_sticker(self: Rc<Self>, conversation: ConversationId, pack: &crate::StickerPackView, sticker: crate::StickerView) {
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
        };
        let Some((id, mut data, timestamp)) = self.store_outgoing(&conversation, String::new(), None, vec![meta], None).await
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
        for contact in storage_contacts.iter().filter(|c| !c.hidden) {
            add(Aci::from(contact.aci).service_id_string(), contact.name.clone(), contact.e164.clone());
        }
        if let Ok(contacts) = self.store.contacts().await {
            for contact in contacts.flatten() {
                let name = Some(contact.name.trim().to_owned()).filter(|n| !n.is_empty());
                add(contact.uuid.to_string(), name, contact.phone_number.map(|p| p.to_string()));
            }
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
        people.remove(&me);
        // The Signal chat's account is made up: it has no profile.
        people.remove(signal_store::SIGNAL_ACI);
        let now = now_millis();
        let mut due = Vec::new();
        for sid in people {
            let has = saved.get(&sid).is_some_and(|p| std::path::Path::new(p).exists());
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
        for (_, _, sid) in due.into_iter().take(AVATAR_CHECKS_PER_SYNC) {
            changed |= self.refresh_person_avatar(manager, &sid, saved.get(&sid).map(String::as_str), dir).await;
        }
        (changed, more)
    }

    /// Fetches their profile and brings their photo up to date: downloaded
    /// when its avatar path differs from the one saved, removed when they
    /// have none. Kept for their 1:1 chat and wherever they appear.
    async fn refresh_person_avatar(&self, manager: &SignalManager, sid: &str, saved: Option<&str>, dir: &std::path::Path) -> bool {
        let Some(aci) = Aci::parse_from_service_id_string(sid) else { return false };
        // Someone known only from a group has their key in the group.
        let Some(key) = self.profile_key_of(&ServiceId::from(aci)).await else { return false };
        // Failures count as a check too, so a profile we can't read (or a
        // rate limit) isn't asked for again until tomorrow.
        let _ = self.db.set_setting(&format!("{SETTING_AVATAR_CHECKED}{sid}"), &now_millis().to_string()).await;
        let path = match crate::profiles::avatar_path(manager, aci, key).await {
            Ok(path) => path,
            Err(error) => {
                debug!(error = %format!("{error:#}"), member = %sid, "profile fetch failed");
                return false;
            }
        };
        let chat = ConversationId::direct(sid);
        let Some(path) = path else {
            // No photo: drop ours, and their chat's (which can predate ours).
            let chat_had = self.db.conversation(&chat).await.ok().flatten().and_then(|c| c.avatar_path);
            if saved.is_none() && chat_had.is_none() {
                return false;
            }
            for old in saved.into_iter().chain(chat_had.as_deref()) {
                let _ = std::fs::remove_file(old);
            }
            let _ = self.db.remove_person_avatar(sid).await;
            let _ = self.db.set_avatar(&chat, None).await;
            return true;
        };
        let stem = crate::profiles::file_stem(sid, &path);
        let current = saved.map(std::path::Path::new).filter(|p| p.exists());
        if let Some(current) = current.filter(|p| p.file_stem().is_some_and(|s| s == stem.as_str())) {
            // Saved while they were only in a group: their 1:1 chat gets it too.
            let chat_has = self.db.conversation(&chat).await.ok().flatten().map(|c| c.avatar_path);
            if let Some(None) = chat_has {
                return self.db.set_avatar(&chat, current.to_str()).await.is_ok();
            }
            return false;
        }
        let bytes = match crate::profiles::download_avatar(manager, &path, key).await {
            Ok(bytes) => bytes,
            Err(error) => {
                debug!(error = %format!("{error:#}"), member = %sid, "profile photo download failed");
                return false;
            }
        };
        let ext = if bytes.starts_with(b"\x89PNG") { "png" } else { "jpg" };
        let file = dir.join(format!("{stem}.{ext}"));
        if let Err(error) = std::fs::write(&file, &bytes) {
            warn!(%error, "failed to save avatar");
            return false;
        }
        let Some(file) = file.to_str() else { return false };
        if let Some(old) = current.filter(|old| *old != std::path::Path::new(file)) {
            let _ = std::fs::remove_file(old);
        }
        let _ = self.db.set_avatar(&chat, Some(file)).await;
        self.db.set_person_avatar(sid, file).await.is_ok()
    }

    /// Brings a group's photo up to date: downloaded when the group's avatar
    /// (a CDN path, new for each upload) differs from the one we saved,
    /// removed when the group has none. `fetch` asks the group server rather
    /// than presage's copy of the group, for right after a change.
    async fn refresh_group_avatar(&self, manager: &SignalManager, key: [u8; 32], fetch: bool) -> bool {
        let conversation = ConversationId::group(&key);
        let stored = if fetch { None } else { self.store.group(key).await.ok().flatten().map(|g| g.avatar) };
        let path = match stored {
            Some(path) => path,
            None => {
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
                        warn!(error = %format!("{error:#}"), "couldn't fetch a group for its photo");
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
            // Signal colors a group by its group id, and its members' names
            // by their place among the sorted member ACIs.
            let id = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key)).get_group_identifier();
            let mut members: Vec<String> = group.members.iter().map(|m| m.aci.service_id_string()).collect();
            members.sort();
            let me = self.self_aci();
            // Our stored copy of a group we've left still lists us.
            let left = self.db.conversation(conversation).await.ok().flatten().is_some_and(|c| c.left);
            let can_edit_info = !left && mapping::can_edit_group_info(&group, &me);
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
            let can_label = !left && mapping::can_set_member_label(&group, &me);
            return Some(ConversationUpdate {
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
        })
    }

    async fn ensure_title(self: &Rc<Self>, conversation: &ConversationId) {
        if self.titled.borrow().contains(conversation) {
            return;
        }
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
            if let Some(update) = self.title_for(&conversation.id).await {
                let differs = update.title.as_ref().is_some_and(|t| *t != conversation.title)
                    || update.kind.is_some_and(|k| k != conversation.kind)
                    || (update.phone.is_some() && update.phone != conversation.phone)
                    || update.color.is_some_and(|c| c != conversation.color)
                    || update.members.as_ref().is_some_and(|m| *m != conversation.members)
                    || update.expire_timer.is_some_and(|t| t != conversation.expire_timer)
                    || update.can_edit_info.is_some_and(|v| v != conversation.can_edit_info)
                    || update.can_label.is_some_and(|v| v != conversation.can_label)
                    || update.admins.as_ref().is_some_and(|a| *a != conversation.admins)
                    || update.member_labels.as_ref().is_some_and(|l| *l != conversation.member_labels)
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
            None => Identifier::ThreadServiceId(conversation.as_direct()?.to_owned()),
        };
        Some(presage::proto::ConversationIdentifier { identifier: Some(identifier) })
    }

    fn addressable(&self, message: &signal_store::Message) -> presage::proto::AddressableMessage {
        use presage::proto::addressable_message::Author;
        presage::proto::AddressableMessage {
            sent_timestamp: Some(message.sent_at as u64),
            author: Some(Author::AuthorServiceId(message.author.clone().unwrap_or_else(|| self.self_aci()))),
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
        let mut body_ranges = mapping::mention_ranges(&wire_body, &mentions);
        body_ranges.extend(mapping::style_ranges(&wire_styles));
        let data = DataMessage {
            body_ranges,
            body: Some(wire_body),
            timestamp: Some(timestamp),
            quote: message.quote.as_ref().map(|q| data_message::Quote {
                id: Some(q.sent_at as u64),
                author_aci: Some(q.author.clone().unwrap_or_else(|| self_aci.clone())),
                text: Some(q.text.clone()),
                attachments: mapping::quoted_attachments(q.attachment.as_ref()),
                r#type: Some(data_message::quote::Type::Normal.into()),
                ..Default::default()
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
        // Mentions go back to placeholders and ranges, as first sent.
        let (wire_body, mentioned) = mapping::placeholders_for(&message.body, &message.mentions);
        let data = DataMessage {
            body_ranges: mapping::mention_ranges(&wire_body, &mentioned),
            body: Some(wire_body).filter(|b| !b.is_empty()),
            timestamp: Some(timestamp),
            quote: message.quote.as_ref().map(|q| data_message::Quote {
                id: Some(q.sent_at as u64),
                author_aci: Some(q.author.clone().unwrap_or_else(|| self_aci.clone())),
                text: Some(q.text.clone()),
                attachments: mapping::quoted_attachments(q.attachment.as_ref()),
                r#type: Some(data_message::quote::Type::Normal.into()),
                ..Default::default()
            }),
            group_v2: self.group_context(&conversation).await,
            ..Default::default()
        };
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
        let mut skipped = 0;
        for conversation in &conversations {
            for message_id in &message_ids {
                let Some(message) = self.db.message(*message_id).await.ok().flatten() else { continue };
                let attachments: Vec<AttachmentMeta> =
                    message.attachments.iter().filter(|a| a.local_path.is_some() && !a.sticker).cloned().collect();
                if message.deleted
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
                "Some messages couldn't be forwarded: stickers, and attachments that haven't downloaded, can't be yet".into(),
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
    async fn send_images(
        self: Rc<Self>,
        conversation: ConversationId,
        images: Vec<crate::OutgoingImage>,
        caption: String,
        quote: Option<i64>,
        mentions: Vec<String>,
        styles: Vec<signal_store::StyleSpan>,
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
            if bytes.len() > crate::MAX_ATTACHMENT_BYTES {
                return self.error("can't send image", "larger than Signal's 100 MB limit");
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
            });
            files.push(Upload { path, file_name: image.file_name, file_type: None, voice: false });
        }
        if files.is_empty() {
            return;
        }
        let wire_caption = caption.trim_end().to_owned();
        let wire_styles = mapping::wire_styles(&wire_caption, &styles);
        let (caption, spans, style_spans) = self.render_body(&wire_caption, &mentions, &wire_styles).await;
        let Some((id, mut data, timestamp)) = self.store_outgoing(&conversation, caption, quote, metas, None).await else {
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
        if file.bytes.len() > crate::MAX_ATTACHMENT_BYTES {
            return self.error("can't send file", "larger than Signal's 100 MB limit");
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
        };
        let wire_caption = caption.trim_end().to_owned();
        let wire_styles = mapping::wire_styles(&wire_caption, &styles);
        let (caption, spans, style_spans) = self.render_body(&wire_caption, &mentions, &wire_styles).await;
        let Some((id, mut data, timestamp)) = self.store_outgoing(&conversation, caption, quote, vec![meta], None).await else {
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
        data.body_ranges = mapping::mention_ranges(&wire_body, mentions);
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
        let conversation = conversation.clone();
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
            attachment: m.attachments.first().map(signal_store::QuotedAttachment::of),
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
        let id = match self.db.insert_message(&message).await {
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
        let data = DataMessage {
            body: Some(body).filter(|b| !b.is_empty()),
            timestamp: Some(timestamp as u64),
            quote: quoted.map(|m| data_message::Quote {
                id: Some(m.sent_at as u64),
                author_aci: Some(m.author.unwrap_or_else(|| self_aci.clone())),
                text: Some(signal_store::hide_spoilers(&m.body, &m.styles)),
                attachments: mapping::quoted_attachments(
                    m.attachments.first().map(signal_store::QuotedAttachment::of).as_ref(),
                ),
                r#type: Some(data_message::quote::Type::Normal.into()),
                ..Default::default()
            }),
            group_v2: self.group_context(&conversation).await,
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

        let data = DataMessage {
            timestamp: Some(timestamp as u64),
            reaction: Some(data_message::Reaction {
                emoji: Some(emoji),
                remove: Some(remove),
                target_author_aci: Some(target.author.unwrap_or_else(|| self.self_aci())),
                target_sent_timestamp: Some(target.sent_at as u64),
                ..Default::default()
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

    async fn delete_for_everyone(self: Rc<Self>, message_id: i64) {
        let Some(target) = self.db.message(message_id).await.ok().flatten() else {
            return;
        };
        let note_to_self = self.is_note_to_self(&target.conversation_id);
        if !crate::deletes::can_send(&target, note_to_self, now_millis()) {
            return self.error("cannot delete for everyone", "only your own sent messages from the last 24 hours");
        }
        if let Err(error) = self.db.mark_deleted(message_id).await {
            return self.error("failed to delete message", error);
        }
        self.emit(Event::MessageChanged { conversation: target.conversation_id.clone(), message_id });
        self.emit(Event::ConversationsChanged);

        let timestamp = self.next_timestamp() as u64;
        let data = DataMessage {
            timestamp: Some(timestamp),
            delete: Some(data_message::Delete { target_sent_timestamp: Some(target.sent_at as u64) }),
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
        // Release notes are only here: no receipts to the Signal chat's
        // made-up account, and nothing for our other devices to sync.
        unread.retain(|(author, _)| author != signal_store::SIGNAL_ACI);
        // Read messages start disappearing now.
        self.expiry.notify_one();
        self.emit(Event::ConversationsChanged);
        if unread.is_empty() {
            return;
        }

        let send_receipts = self.setting_enabled(SETTING_READ_RECEIPTS).await;
        let mut by_author: HashMap<String, Vec<u64>> = HashMap::new();
        for (author, sent_at) in &unread {
            by_author.entry(author.clone()).or_default().push(*sent_at as u64);
        }
        if send_receipts {
            for (author, timestamps) in by_author {
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

        let read = unread
            .into_iter()
            .map(|(author, sent_at)| sync_message::Read {
                sender_aci: Some(author),
                timestamp: Some(sent_at as u64),
                ..Default::default()
            })
            .collect();
        self.enqueue(Outbound::Sync {
            body: SyncMessage { read, ..SyncMessage::with_padding(&mut rand::rng()) },
        });
    }

    async fn typing(self: Rc<Self>, conversation: ConversationId, started: bool) {
        if !self.setting_enabled(SETTING_TYPING_INDICATORS).await
            || conversation.as_direct() == Some(self.self_aci().as_str())
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

    async fn setting_enabled(&self, key: &str) -> bool {
        !matches!(self.db.setting(key).await, Ok(Some(v)) if v == "false")
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
                    _ => {}
                }
                continue;
            };
            match job {
                Outbound::Send { conversation, body, timestamp, message } => {
                    let status = self.deliver(&mut manager, &conversation, body, timestamp).await;
                    if let Some(id) = message {
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
                Outbound::Sync { body } => {
                    let me = self.self_aci();
                    let Some(me) = ServiceId::parse_from_service_id_string(&me) else { continue };
                    if let Err(error) = manager.send_message(me, body, self.next_timestamp() as u64).await {
                        warn!(%error, "failed to send sync message");
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
        // Its account is made up (as Desktop's): nothing goes to it.
        if conversation.is_signal() {
            warn!("not sending to the Signal chat");
            return DeliveryStatus::Failed;
        }
        let body = self.stamp_timer(conversation, body).await;
        let result = match (conversation.as_group(), mapping::service_id_of(conversation)) {
            (Some(key), _) => manager.send_message_to_group(&key, body, timestamp).await,
            (None, Some(recipient)) => manager.send_message(recipient, body, timestamp).await,
            (None, None) => {
                self.error("cannot send", format!("invalid conversation {conversation}"));
                return DeliveryStatus::Failed;
            }
        };
        match result {
            Ok(()) => DeliveryStatus::Sent,
            Err(error) => {
                self.error("failed to send", error);
                DeliveryStatus::Failed
            }
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
        let bytes = tokio::fs::read(path).await?;
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
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
    }

    /// Applies a read sync that came before this message (or edit), if one
    /// did; `true` when it's now read. Kept for later if applying fails.
    async fn apply_early_read(&self, author: &str, sent_at: i64) -> bool {
        let position = |early: &std::collections::VecDeque<(String, i64)>| {
            early.iter().position(|(a, at)| a == author && *at == sent_at)
        };
        if position(&self.early_reads.borrow()).is_none() {
            return false;
        }
        match self.db.apply_read_sync(author, sent_at).await {
            Ok(_) => {
                let mut early = self.early_reads.borrow_mut();
                if let Some(ix) = position(&early) {
                    early.remove(ix);
                }
                self.expiry.notify_one();
                true
            }
            Err(error) => {
                self.error("failed to apply read sync", error);
                false
            }
        }
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
        Ok(crate::SafetyNumber { digits, qr, verified })
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
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::Verified(presage::proto::Verified {
                    destination_aci: Some(theirs.service_id_string()),
                    destination_aci_binary: Some(theirs.service_id_binary()),
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
        let pointer = data_message_of(&content.body).and_then(|dm| dm.attachments.get(index).cloned());
        let Some(pointer) = pointer else {
            return self.error("attachment unavailable", "no such attachment");
        };
        self.download(&manager, message.conversation_id, message_id, index, &pointer).await;
    }

    async fn download(
        &self,
        manager: &SignalManager,
        conversation: ConversationId,
        message_id: i64,
        index: usize,
        pointer: &AttachmentPointer,
    ) {
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
            Ok(()) => self.emit(Event::MessageChanged { conversation, message_id }),
            Err(error) => self.error("failed to record attachment", error),
        }
    }
}

impl Actor {
    /// Downloads an attachment from its saved transit-tier locator.
    async fn download_remote(
        &self,
        manager: &SignalManager,
        conversation: &ConversationId,
        message_id: i64,
        index: usize,
        meta: &AttachmentMeta,
    ) -> anyhow::Result<()> {
        let remote = meta.remote.as_ref().context("no locator")?;
        let bytes = crate::history::download_attachment(manager, remote, meta.size).await?;
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
        self.db.set_attachment_path(message_id, index, &path.to_string_lossy()).await?;
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

    /// Photos and GIFs from transferred history, one at a time. Ones that
    /// fail (expired from the transit tier, usually) stay downloadable by
    /// hand, which says why.
    async fn download_transferred(self: Rc<Self>, manager: SignalManager, downloads: Vec<(i64, usize)>) {
        let total = downloads.len();
        let mut failed = 0;
        for (message_id, index) in downloads {
            let Some(message) = self.db.message(message_id).await.ok().flatten() else { continue };
            let Some(meta) = message.attachments.get(index).filter(|a| a.local_path.is_none()) else { continue };
            if let Err(error) = self.download_remote(&manager, &message.conversation_id, message_id, index, meta).await {
                debug!(error = %format!("{error:#}"), "transferred attachment download failed");
                failed += 1;
            }
        }
        info!(total, failed, "downloaded transferred photos");
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
