//! Signal networking on a dedicated Tokio runtime.
//!
//! The UI never touches presage directly: it sends [`Command`]s and receives
//! [`Event`]s, and reads rendered state from [`signal_store::AppDb`].

use std::{future::Future, path::PathBuf};

use anyhow::Context as _;
use futures::channel::mpsc;
use signal_store::{AppDb, ConversationId};

pub use url::Url;

mod actor;
mod demo;
pub mod mapping;
mod storage;
mod groups;
mod history;
mod safety;
mod calling;
mod profiles;
pub mod media;
pub mod link_preview;
pub mod debug_log;
pub mod recipients;
pub mod giphy;
pub mod sticker_upload;
pub mod deletes;
pub mod edits;
pub mod release_notes;

/// Most conversations an account can pin (Signal's limit).
pub use storage::MAX_PINNED;

/// Settings key: when `"false"`, read receipts are not sent.
pub const SETTING_READ_RECEIPTS: &str = "read_receipts";
/// Settings key: when `"false"`, typing indicators are not sent.
pub const SETTING_TYPING_INDICATORS: &str = "typing_indicators";
/// Settings key: when `"false"`, link previews aren't generated (synced from
/// the account's Storage Service record).
pub const SETTING_LINK_PREVIEWS: &str = "link_previews";
/// Settings key: your Giphy API key for GIF search (`GIPHY_API_KEY` also
/// works). Not a Signal setting.
pub const SETTING_GIPHY_API_KEY: &str = "giphy_api_key";

/// Settings key: lossless optimization of images we send (`max`, `fast` or
/// `off`; see [`media::Optimization`]). Not a Signal setting.
pub const SETTING_IMAGE_OPTIMIZATION: &str = "image_optimization";

pub use link_preview::{LinkPreviewDraft, PreviewImage};

/// TLS for Signal's own servers (the calling server, updates2.signal.org),
/// whose certificates come from Signal's root CA: trusting only that, as
/// Desktop does.
pub(crate) fn signal_tls() -> anyhow::Result<rustls::ClientConfig> {
    use rustls::pki_types::{CertificateDer, pem::PemObject as _};
    let pem = presage::libsignal_service::configuration::ServiceConfiguration::from(presage::libsignal_service::configuration::SignalServers::Production)
        .certificate_authority;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(cert?)?;
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding `signal.db` (presage protocol state), `app.db` and attachments.
    pub data_dir: PathBuf,
    /// Raw SQLCipher key for both databases.
    pub db_key: [u8; 32],
    /// Device name shown in the phone's "Linked devices" list.
    pub device_name: String,
    /// Demo mode: sample data, never connects to Signal (see `demo.rs`).
    pub demo: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Starts linking as a secondary device. Emits `Event::LinkUrl` then `Event::Linked`.
    Link,
    SendText {
        conversation: ConversationId,
        /// With a [`mapping::MENTION_PLACEHOLDER`] per @mention.
        body: String,
        /// Message id (in `AppDb`) being replied to.
        quote: Option<i64>,
        /// A preview fetched with [`Command::FetchLinkPreview`], to send along.
        preview: Option<LinkPreviewDraft>,
        /// Who each placeholder in `body` mentions, in order.
        mentions: Vec<String>,
        /// Its formatting: spans over `body` (placeholders and all).
        styles: Vec<signal_store::StyleSpan>,
    },
    /// GIF search (trending when `query` is empty); answered with `Event::Gifs`.
    SearchGifs { query: String, offset: u32 },
    /// Sends a GIF from search results.
    SendGif { conversation: ConversationId, gif: giphy::Gif },
    /// Lists installed sticker packs; answered with `Event::Stickers`.
    LoadStickers,
    /// Uploads a new sticker pack and installs it; answered with
    /// `Event::StickerUpload`.
    UploadStickerPack(sticker_upload::NewStickerPack),
    /// Sends a sticker from an installed pack.
    SendSticker {
        conversation: ConversationId,
        pack: std::sync::Arc<StickerPackView>,
        sticker: StickerView,
    },
    /// Looks up a username or phone number on Signal; answered with
    /// `Event::RecipientFound`.
    FindRecipient { query: String },
    /// Builds a link preview for `url`; answered with `Event::LinkPreview`.
    FetchLinkPreview { url: String },
    /// Sends images (optimized losslessly first), with `caption` as the
    /// message text.
    SendImages {
        conversation: ConversationId,
        images: Vec<OutgoingImage>,
        caption: String,
        quote: Option<i64>,
        /// Who each placeholder in `caption` mentions, in order.
        mentions: Vec<String>,
        /// Its formatting: spans over `caption` (placeholders and all).
        styles: Vec<signal_store::StyleSpan>,
    },
    /// Sends any file as it is (not optimized), with `caption` as the
    /// message text.
    SendFile {
        conversation: ConversationId,
        file: OutgoingFile,
        caption: String,
        quote: Option<i64>,
        /// Who each placeholder in `caption` mentions, in order.
        mentions: Vec<String>,
        /// Its formatting: spans over `caption` (placeholders and all).
        styles: Vec<signal_store::StyleSpan>,
    },
    /// Reacts to message `message_id`; `remove` retracts our reaction.
    React { message_id: i64, emoji: String, remove: bool },
    /// Marks a conversation read locally and sends read receipts / sync.
    MarkRead { conversation: ConversationId },
    /// Sends a typing indicator.
    Typing { conversation: ConversationId, started: bool },
    /// Deletes messages on this device and your other devices.
    DeleteForMe { message_ids: Vec<i64> },
    /// Pins a message for everyone in the chat; `duration` in seconds,
    /// `None` for forever.
    PinMessage { message_id: i64, duration: Option<u32> },
    UnpinMessage { message_id: i64 },
    /// Changes the text of one of our sent messages (Signal's edit).
    EditMessage { message_id: i64, body: String, mentions: Vec<String>, styles: Vec<signal_store::StyleSpan> },
    /// Sends a failed message again, with its original timestamp.
    RetrySend { message_id: i64 },
    /// Sends copies of messages (text, photos, GIFs, link previews) to
    /// other chats, as Desktop's "Forward".
    Forward { message_ids: Vec<i64>, conversations: Vec<ConversationId> },
    /// Deletes a whole chat on this device and your other devices.
    DeleteConversation { conversation: ConversationId },
    /// Deletes our own message for everyone.
    DeleteForEveryone { message_id: i64 },
    /// Downloads attachment `index` of `message_id`.
    FetchAttachment { message_id: i64, index: usize },
    /// Pins or unpins a conversation for the whole account (Storage Service).
    SetPinned { conversation: ConversationId, pinned: bool },
    /// Archives, mutes, marks unread or blocks a chat for the whole account
    /// (Storage Service).
    SetChat { conversation: ConversationId, change: ChatChange },
    /// Sets a chat's disappearing-message timer (seconds; 0 turns it off)
    /// for everyone in it.
    SetExpireTimer { conversation: ConversationId, seconds: u32 },
    /// Renames a group or changes its description (`None`: unchanged).
    UpdateGroup { conversation: ConversationId, title: Option<String>, description: Option<String> },
    /// Leaves a group.
    LeaveGroup { conversation: ConversationId },
    /// Creates a group with us as its admin; answered with `Event::GroupCreated`.
    CreateGroup { title: String, members: Vec<String> },
    /// Sets (or with an empty `text`, clears) our label in a group.
    SetMemberLabel { conversation: ConversationId, emoji: Option<String>, text: String },
    /// Computes a 1:1 chat's safety number; answered with `Event::SafetyNumber`.
    LoadSafetyNumber { conversation: ConversationId },
    /// Marks a 1:1 contact's current identity verified (or clears it), here
    /// and on our other devices; answered with `Event::SafetyNumber`.
    SetVerified { conversation: ConversationId, verified: bool },
    /// Calls a 1:1 chat (Desktop's call buttons, after the lobby).
    StartCall { conversation: ConversationId, video: bool },
    /// Answers the ringing call, with our camera on or off.
    AcceptCall { video: bool },
    /// Declines the ringing call.
    DeclineCall,
    /// Ends the call (or stops calling).
    HangUp,
    /// Mutes (`false`) or unmutes our microphone.
    SetCallAudio(bool),
    /// Turns our camera on or off: in a call, or for the lobby's preview.
    SetCallVideo(bool),
    /// Who's in a group's call, for the lobby; answered with `Event::GroupPeek`.
    PeekGroupCall { conversation: ConversationId },
    /// Lists screens and windows to present; answered with `Event::ShareSources`.
    LoadShareSources,
    /// Presents a screen or window (an id from `Event::ShareSources`).
    StartSharing { source: String },
    /// Stops presenting; the camera comes back if it was on.
    StopSharing,
    /// Lists microphones, speakers and cameras; answered with `Event::CallDevices`.
    LoadCallDevices,
    /// Chooses a device for calls (`id` from `Event::CallDevices`), saved.
    SetCallDevice { kind: DeviceKind, id: String },
    /// Unlinks this device and wipes local state.
    Unlink,
    /// Stops waiting for the history transfer and continues without it.
    /// Ignored once the import has started.
    SkipHistory,
}

/// A per-chat setting kept in the Storage Service.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatChange {
    Archived(bool),
    /// Unix millis; [`MUTED_ALWAYS`] for always, 0 unmutes.
    MutedUntil(i64),
    MarkedUnread(bool),
    Blocked(bool),
}

/// Pinned messages per chat (Desktop's `global.pinnedMessageLimit` default).
pub const MAX_PINNED_MESSAGES: usize = 3;

/// The history transfer after linking, as shown while it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryProgress {
    /// Waiting for the phone to upload it.
    Waiting,
    /// Bytes downloaded, of `total` when the CDN says.
    Downloading { done: u64, total: Option<u64> },
    /// Bytes of the decrypted backup imported so far.
    Importing { done: u64, total: u64 },
    Done { chats: usize, messages: usize },
    /// The phone skipped it.
    Skipped,
    Failed(String),
}

/// "Muted always", as Signal's apps store it (Android's `Long.MAX_VALUE`).
pub const MUTED_ALWAYS: i64 = i64::MAX;

/// A page of GIF search results.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GifResults {
    /// Each GIF with its downloaded preview.
    pub gifs: Vec<(giphy::Gif, std::path::PathBuf)>,
    pub next: Option<u32>,
}

/// Your Giphy key: Settings, then `GIPHY_API_KEY`.
pub(crate) async fn giphy_key(db: &AppDb) -> Option<String> {
    let clean = |key: String| Some(key.trim().to_owned()).filter(|k| !k.is_empty());
    // A cleared setting falls back to the environment.
    db.setting(SETTING_GIPHY_API_KEY)
        .await
        .ok()
        .flatten()
        .and_then(clean)
        .or_else(|| std::env::var("GIPHY_API_KEY").ok().and_then(clean))
}

/// Searches Giphy and downloads the result previews into `cache`.
pub(crate) async fn search_gifs(db: &AppDb, cache: &std::path::Path, query: &str, offset: u32) -> Result<GifResults, String> {
    let key = giphy_key(db).await.ok_or("Add a Giphy API key to search GIFs")?;
    let page = giphy::search(&key, query, offset).await.map_err(|e| format!("{e:#}"))?;
    std::fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let downloads = page.gifs.into_iter().map(|gif| async move {
        let path = cache.join(format!("{}.mp4", gif.id.replace(['/', '.'], "")));
        if !path.exists() {
            let bytes = giphy::download(&gif.preview.url).await.ok()?;
            // Whole files only: an interrupted write mustn't be reused.
            let partial = path.with_extension(format!("{}.part", rand::Rng::random::<u32>(&mut rand::rng())));
            tokio::fs::write(&partial, bytes).await.ok()?;
            tokio::fs::rename(&partial, &path).await.ok()?;
        }
        Some((gif, path))
    });
    let gifs = futures::future::join_all(downloads).await.into_iter().flatten().collect();
    Ok(GifResults { gifs, next: page.next })
}

/// An installed sticker pack, with its stickers saved to disk for display.
#[derive(Debug, Clone, PartialEq)]
pub struct StickerPackView {
    pub id: Vec<u8>,
    pub key: Vec<u8>,
    pub title: String,
    pub author: String,
    pub stickers: Vec<StickerView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickerView {
    pub id: u32,
    pub emoji: Option<String>,
    pub path: std::path::PathBuf,
}

/// An image picked, pasted or dropped in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingImage {
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub file_name: Option<String>,
}

/// The call in progress, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallUpdate {
    pub conversation: ConversationId,
    pub status: CallStatus,
    /// Started (or offered) as a video call.
    pub video: bool,
    /// They called us.
    pub incoming: bool,
    /// Their camera, and their microphone, are on.
    pub remote_video: bool,
    pub remote_audio: bool,
    /// We're presenting our screen; they're presenting theirs.
    pub sharing: bool,
    pub remote_sharing: bool,
    /// A group call; its participants (not us), in Desktop's order.
    pub group: bool,
    pub participants: Vec<CallParticipant>,
    /// Who's ringing us, for a group call's ring (their ACI).
    pub ringer: Option<String>,
}

/// Someone in a group call (one of their devices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallParticipant {
    /// Their ACI.
    pub aci: String,
    /// Their video's frames: `CallVideo::group.slot(demux_id)`.
    pub demux_id: u32,
    pub video: bool,
    pub audio: bool,
    pub presenting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallStatus {
    /// They're calling; not answered yet.
    Ringing,
    /// We're calling: Desktop's "Connecting…", then "Ringing…".
    Calling,
    RingingThem,
    /// Answered; the connection is being set up (or recovering).
    Connecting,
    /// Since this time (unix millis).
    Connected { since: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Microphone,
    Speaker,
    Camera,
}

impl DeviceKind {
    /// Where the choice is saved.
    pub(crate) fn setting(self) -> &'static str {
        match self {
            Self::Microphone => "calls.microphone",
            Self::Speaker => "calls.speaker",
            Self::Camera => "calls.camera",
        }
    }
}

/// The devices calls can use; a chosen one is `None` for the system default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallDevices {
    pub devices: signal_calls::Devices,
    pub microphone: Option<String>,
    pub speaker: Option<String>,
    pub camera: Option<String>,
}

pub use signal_calls::{Device, Devices, ShareSource};

/// The call's video: their latest frame, and our camera's (mirrored).
#[derive(Clone, Default)]
pub struct CallVideo {
    pub remote: signal_calls::FrameSlot,
    pub local: signal_calls::FrameSlot,
    /// A group call's participants' frames.
    pub group: signal_calls::GroupFrames,
}

pub use signal_calls::{Frame, FrameSlot};

/// A 1:1 contact's safety number, as Signal shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyNumber {
    /// 60 digits, shown in 12 groups of five.
    pub digits: String,
    /// The scannable fingerprint, for the QR code.
    pub qr: Vec<u8>,
    /// We marked their current identity key verified.
    pub verified: bool,
}

/// A file (not sent as an image) picked, pasted or dropped in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingFile {
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub file_name: String,
    /// A recorded voice message (AAC in an M4A file), sent as one.
    pub voice: bool,
}

/// What a voice message is sent as: AAC (in an M4A file), as Signal's
/// phones send theirs.
pub const VOICE_CONTENT_TYPE: &str = "audio/aac";

/// Signal's attachment size limit.
pub const MAX_ATTACHMENT_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    /// Connected and draining the server queue.
    Syncing,
    Online,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// No account on disk; the UI should show the link screen.
    NeedsLinking,
    /// Provisioning URL to render as a QR code.
    LinkUrl(Url),
    LinkFailed(String),
    /// Account is ready. Emitted at startup when already linked, and after linking.
    Ready { phone: Option<String>, aci: String },
    Connection(ConnectionState),
    /// Conversation list rows changed (title, preview, unread, ...).
    ConversationsChanged,
    /// A message was inserted or updated (status, reaction, edit, delete, attachment).
    MessageChanged { conversation: ConversationId, message_id: i64 },
    Typing { conversation: ConversationId, author: String, started: bool },
    /// A message is gone (deleted for me, here or on another device).
    MessageRemoved { conversation: ConversationId, message_id: i64 },
    /// A whole chat is gone.
    ConversationRemoved(ConversationId),
    /// Some of a chat's messages are gone (deleted on another device); reload it.
    ConversationCleared(ConversationId),
    /// A chat's pinned messages changed.
    PinsChanged(ConversationId),
    /// Who's in a group's call (their ACIs, one per device; empty: no call).
    GroupPeek { conversation: ConversationId, users: Vec<String> },
    /// Screens and windows to present, or why there are none.
    ShareSources(Result<Vec<signal_calls::ShareSource>, String>),
    /// The devices calls can use, and which are chosen.
    CallDevices(CallDevices),
    /// Our camera couldn't start, or stopped on its own: why.
    CameraFailed(String),
    /// The call changed (`None`: no call any more).
    Call(Option<CallUpdate>),
    /// A 1:1 chat's safety number, or why there isn't one.
    SafetyNumber { conversation: ConversationId, result: Result<SafetyNumber, String> },
    /// A group we created is ready to open.
    GroupCreated(ConversationId),
    /// Progress of the message history transfer after linking.
    /// Draining the server queue after starting: the server timestamps
    /// (unix millis) of the first queued message and the latest one
    /// processed. Sent a few times a second at most, and only until the first
    /// `Connection(Online)`.
    CatchingUp { first: i64, latest: i64 },
    History(HistoryProgress),
    /// A new message from someone else arrived (for notifications).
    Incoming { conversation: ConversationId, message_id: i64, mentions_me: bool },
    /// GIF search results with their previews downloaded (`Err`: why not).
    Gifs { query: String, offset: u32, result: Result<GifResults, String> },
    /// Installed sticker packs (answer to `Command::LoadStickers`).
    Stickers(Vec<StickerPackView>),
    /// How a `Command::UploadStickerPack` is going.
    StickerUpload(sticker_upload::StickerUpload),
    /// Answer to `Command::FindRecipient`: who it is, or why not.
    RecipientFound { query: String, result: Result<signal_store::DirectoryEntry, String> },
    /// Answer to `Command::FetchLinkPreview` (`None`: no preview for it).
    LinkPreview { url: String, preview: Option<LinkPreviewDraft> },
    /// Stored author names changed: `(service id, name)` pairs. Loaded
    /// messages and quotes by these authors should take the new names.
    AuthorNamesChanged(Vec<(String, String)>),
    Error(String),
    Unlinked,
}

/// The backend thread's stack (see `Backend::start`).
const BACKEND_STACK: usize = 16 * 1024 * 1024;

/// Handle to the running backend. Cheap to clone.
#[derive(Clone)]
pub struct Backend {
    db: AppDb,
    video: CallVideo,
    commands: mpsc::UnboundedSender<Command>,
    runtime: tokio::runtime::Handle,
    data_dir: std::path::PathBuf,
}

impl Backend {
    /// Where the databases, attachments and logs live.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Opens the databases, starts the runtime thread, and begins connecting
    /// (or emits `NeedsLinking`). Events arrive on the returned receiver, which
    /// may be polled from any executor.
    pub fn start(config: Config) -> anyhow::Result<(Backend, mpsc::UnboundedReceiver<Event>)> {
        std::fs::create_dir_all(config.data_dir.join("attachments"))
            .with_context(|| format!("creating {}", config.data_dir.display()))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("signal-worker")
            .enable_all()
            .build()
            .context("starting tokio runtime")?;

        let db = runtime
            .block_on(AppDb::open(&config.data_dir.join("app.db"), &config.db_key))
            .context("opening app database")?;
        if let Err(error) = runtime.block_on(db.upgrade_colors()) {
            tracing::warn!(%error, "failed to recompute avatar colors");
        }
        if let Err(error) = runtime.block_on(db.upgrade_previews()) {
            tracing::warn!(%error, "failed to refresh conversation previews");
        }

        // Never let the pool drop implicitly: see `actor::run` on SQLCipher's
        // exit-time shutdown. `Backend::shutdown` closes it explicitly.
        std::mem::forget(db.clone());

        let (commands, command_rx) = mpsc::unbounded();
        let (event_tx, events) = mpsc::unbounded();
        let video = CallVideo::default();
        let backend = Backend {
            db: db.clone(),
            video: video.clone(),
            commands,
            runtime: runtime.handle().clone(),
            data_dir: config.data_dir.clone(),
        };

        std::thread::Builder::new()
            .name("signal-backend".into())
            // Decrypting a message runs deep, generic futures (presage's and
            // libsignal-service's, built in this crate); unoptimized in debug
            // builds, they overflowed the default 2 MB stack.
            .stack_size(BACKEND_STACK)
            .spawn(move || {
                // presage's futures are !Send, so the actor lives on a LocalSet
                // pinned to this thread; `Backend::spawn` work goes to the workers.
                let local = tokio::task::LocalSet::new();
                if config.demo {
                    runtime.block_on(local.run_until(demo::run(db, config.data_dir.clone(), command_rx, event_tx)));
                } else {
                    runtime.block_on(local.run_until(actor::run(config, db, video, command_rx, event_tx)));
                }
                // `Backend::spawn` and `shutdown` use this runtime's handle
                // even if the actor stopped early (signal.db failed to open,
                // say): never shut it down.
                std::mem::forget(runtime);
            })
            .context("spawning backend thread")?;

        Ok((backend, events))
    }

    /// Closes the app database (checkpointing its WAL) before the process
    /// exits. Blocks for at most two seconds; call from a non-runtime thread.
    pub fn shutdown(&self) {
        let db = self.db.clone();
        let _ = self
            .runtime
            .block_on(async move { tokio::time::timeout(std::time::Duration::from_secs(2), db.close()).await });
    }

    pub fn db(&self) -> &AppDb {
        &self.db
    }

    /// The call's video frames.
    pub fn call_video(&self) -> &CallVideo {
        &self.video
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.unbounded_send(command);
    }

    /// Runs `fut` on the backend runtime (needed for `AppDb` queries) and
    /// returns a future that can be awaited from any executor.
    pub fn spawn<F>(&self, fut: F) -> impl Future<Output = F::Output> + Send + 'static
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.runtime.spawn(fut);
        async move { handle.await.expect("backend task panicked") }
    }
}
