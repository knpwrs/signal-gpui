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
mod demo_groups;
pub mod mapping;
mod storage;
mod groups;
pub mod group_admin;
mod history;
mod backup_media;
mod safety;
mod key_transparency;
mod calling;
mod call_links;
mod profiles;
pub mod media;
pub mod link_preview;
pub mod debug_log;
pub mod recipients;
pub mod giphy;
pub mod sticker_upload;
pub mod deletes;
pub mod edits;
pub mod long_text;
pub mod release_notes;
pub mod megaphones;
pub mod remote_config;
pub mod message_requests;
mod sender_keys;

pub use remote_config::{RemoteConfig, current as remote_config};

/// Settings key: `"true"` when read receipts are on: sent, and others'
/// shown. See [`AccountSetting`] for where it comes from and its default.
pub const SETTING_READ_RECEIPTS: &str = "read_receipts";
/// Settings key: `"true"` when typing indicators are on: sent, and others'
/// shown.
pub const SETTING_TYPING_INDICATORS: &str = "typing_indicators";
/// Settings key: `"true"` when link previews are generated.
pub const SETTING_LINK_PREVIEWS: &str = "link_previews";
/// Settings key: `"true"` to show which messages came by sealed sender
/// (Desktop's "Show status icon"). Kept for the account record; nothing
/// shows it here yet.
pub const SETTING_SEALED_SENDER_INDICATORS: &str = "sealed_sender_indicators";

/// One of the account's privacy settings that Signal syncs between devices
/// (the `AccountRecord` in Storage Service, the configuration sync message
/// and the link-time backup). Like Desktop (`ts/util/Settings.preload.ts`),
/// each is off until one of those says otherwise; read receipts also take
/// the phone's setting at linking (`AccountManager`'s `createAccount`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountSetting {
    ReadReceipts,
    TypingIndicators,
    LinkPreviews,
    SealedSenderIndicators,
}

impl AccountSetting {
    pub const ALL: [AccountSetting; 4] =
        [Self::ReadReceipts, Self::TypingIndicators, Self::LinkPreviews, Self::SealedSenderIndicators];

    /// Where it's kept in the app database's settings.
    pub fn key(self) -> &'static str {
        match self {
            Self::ReadReceipts => SETTING_READ_RECEIPTS,
            Self::TypingIndicators => SETTING_TYPING_INDICATORS,
            Self::LinkPreviews => SETTING_LINK_PREVIEWS,
            Self::SealedSenderIndicators => SETTING_SEALED_SENDER_INDICATORS,
        }
    }
}

/// Whether a stored [`AccountSetting`] is on: only `"true"` is (unset is
/// Desktop's default, off).
pub fn setting_on(value: Option<&str>) -> bool {
    value == Some("true")
}

/// Account settings as one source gives them; `None` leaves one as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccountSettings {
    pub read_receipts: Option<bool>,
    pub typing_indicators: Option<bool>,
    pub link_previews: Option<bool>,
    pub sealed_sender_indicators: Option<bool>,
}

impl AccountSettings {
    /// Each setting this gives a value for.
    pub fn values(&self) -> impl Iterator<Item = (AccountSetting, bool)> {
        [
            (AccountSetting::ReadReceipts, self.read_receipts),
            (AccountSetting::TypingIndicators, self.typing_indicators),
            (AccountSetting::LinkPreviews, self.link_previews),
            (AccountSetting::SealedSenderIndicators, self.sealed_sender_indicators),
        ]
        .into_iter()
        .filter_map(|(setting, on)| Some((setting, on?)))
    }
}
/// Settings key: your Giphy API key for GIF search (`GIPHY_API_KEY` also
/// works). Not a Signal setting.
pub const SETTING_GIPHY_API_KEY: &str = "giphy_api_key";

/// Settings key: when `"true"`, stories are turned off (Desktop's
/// `hasStoriesDisabled`): none are kept or shown.
pub const SETTING_STORIES_DISABLED: &str = "stories_disabled";
/// Settings key: `"false"` to not tell people we viewed their stories
/// (Desktop's `storyViewReceiptsEnabled`; unset follows read receipts).
pub const SETTING_STORY_VIEW_RECEIPTS: &str = "story_view_receipts";

/// Settings key: lossless optimization of images we send (`max`, `fast` or
/// `off`; see [`media::Optimization`]). Not a Signal setting.
pub const SETTING_IMAGE_OPTIMIZATION: &str = "image_optimization";

pub use link_preview::{LinkPreviewDraft, PreviewImage};

/// Settings key: `"true"` when automatic key verification is off.
pub const SETTING_KEY_TRANSPARENCY_DISABLED: &str = key_transparency::SETTING_DISABLED;
/// Settings key: `"true"` once Desktop's key transparency introduction was seen.
pub const SETTING_KEY_TRANSPARENCY_ONBOARDING_SEEN: &str = key_transparency::SETTING_ONBOARDING_SEEN;
/// Whether key transparency is on at all (Desktop's remote config flag
/// `desktop.keyTransparency.prod`).
pub fn key_transparency_on() -> bool {
    key_transparency::feature_on()
}

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

/// A group a link leads to, as Desktop's `GroupV2JoinDialog` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLinkPreview {
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    pub member_count: u32,
    /// An admin approves who joins.
    pub approval: bool,
    /// The group's photo, downloaded.
    pub avatar: Option<PathBuf>,
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
    /// Sends a GIF from search results, replying to `quote` (our row id)
    /// when given.
    SendGif { conversation: ConversationId, gif: giphy::Gif, quote: Option<i64> },
    /// Lists installed sticker packs; answered with `Event::Stickers`.
    LoadStickers,
    /// Uploads a new sticker pack and installs it; answered with
    /// `Event::StickerUpload`.
    UploadStickerPack(sticker_upload::NewStickerPack),
    /// Sends a sticker from an installed pack, replying to `quote` (our
    /// row id) when given.
    SendSticker {
        conversation: ConversationId,
        pack: std::sync::Arc<StickerPackView>,
        sticker: StickerView,
        quote: Option<i64>,
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
    /// Sends a poll (Desktop's "New poll"); empty options are left out.
    SendPoll { conversation: ConversationId, question: String, options: Vec<String>, allow_multiple: bool },
    /// Votes in poll `message_id` for these options (none takes our vote
    /// back).
    VotePoll { message_id: i64, options: Vec<u32> },
    /// Ends our poll `message_id` for everyone.
    EndPoll { message_id: i64 },
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
    /// Answers a message request as Desktop's message request bar does:
    /// `Accept` (also what Unblock does there), `Delete`, `Block`,
    /// `Spam` (Report spam) or `BlockAndSpam` (Report and block). Here
    /// and on our other devices; a spam report goes to Signal. Answered
    /// with `Event::MessageRequestAnswered`.
    AnswerMessageRequest { conversation: ConversationId, answer: signal_store::MessageRequestResponse },
    /// Deletes our own message for everyone.
    DeleteForEveryone { message_id: i64 },
    /// Downloads attachment `index` of `message_id`.
    FetchAttachment { message_id: i64, index: usize },
    /// Opens their view-once message, its one time: answered with
    /// `Event::ViewOnceOpened`, after which it's erased.
    OpenViewOnce { message_id: i64 },
    /// Sends a photo or video to be viewed once (Desktop's view-once
    /// toggle): no caption, no quote.
    SendViewOnce { conversation: ConversationId, media: OutgoingViewOnce },
    /// Downloads the whole text of a long message whose download failed
    /// (Desktop's "Download Full Message").
    FetchLongText { message_id: i64 },
    /// Pins or unpins a conversation for the whole account (Storage Service).
    SetPinned { conversation: ConversationId, pinned: bool },
    /// Archives, mutes, marks unread or blocks a chat for the whole account
    /// (Storage Service).
    SetChat { conversation: ConversationId, change: ChatChange },
    /// Sets (or, all empty, deletes) the nickname and note we give someone
    /// in a 1:1 chat, here and for the whole account (Storage Service), as
    /// Desktop's "Nickname" dialog does. `plain_name` is ignored.
    SetNickname { conversation: ConversationId, nickname: signal_store::ContactNickname },
    /// Sets a chat's disappearing-message timer (seconds; 0 turns it off)
    /// for everyone in it.
    SetExpireTimer { conversation: ConversationId, seconds: u32 },
    /// Renames a group or changes its description (`None`: unchanged).
    UpdateGroup { conversation: ConversationId, title: Option<String>, description: Option<String> },
    /// Leaves a group.
    LeaveGroup { conversation: ConversationId },
    /// Creates a group with us as its admin; answered with `Event::GroupCreated`.
    /// `avatar` is a picked image (made a group photo as Desktop does) and
    /// `expire_timer` the disappearing message timer, in seconds (0: off).
    CreateGroup { title: String, members: Vec<String>, avatar: Option<Vec<u8>>, expire_timer: u32 },
    /// Sets the group's photo from a picked image, or removes it (`None`).
    SetGroupAvatar { conversation: ConversationId, image: Option<Vec<u8>> },
    /// Removes someone from a group, as Desktop's `removeFromGroupV2` does:
    /// a member (banned from rejoining by the link), an invitation, or a
    /// request to join (turned down). `service_id` is an ACI, or a PNI for
    /// an invitation.
    RemoveFromGroup { conversation: ConversationId, service_id: String },
    /// Makes a member an admin, or not (Desktop's `toggleAdmin`).
    SetGroupAdmin { conversation: ConversationId, member: String, admin: bool },
    /// Who may edit details, add people or send messages.
    SetGroupPermission { conversation: ConversationId, permission: group_admin::Permission },
    /// Turns the group link on or off, resets it, or sets admin approval.
    ChangeGroupLink { conversation: ConversationId, change: group_admin::LinkChange },
    /// Approves or turns down a request to join (an ACI).
    AnswerJoinRequest { conversation: ConversationId, member: String, approve: bool },
    /// Revokes invitations (service ids).
    RevokeGroupInvites { conversation: ConversationId, invitees: Vec<String> },
    /// Takes back our request to join a group.
    CancelJoinRequest { conversation: ConversationId },
    /// A `signal.group` link was opened: answered with
    /// `Event::GroupLinkPreview` (Desktop's join dialog), `Event::GroupOpened`
    /// when we're already in (or asking to join) the group, or
    /// `Event::GroupLinkFailed`.
    OpenGroupLink { url: String },
    /// Joins (or asks to join) the group a link names; answered with
    /// `Event::GroupOpened` or `Event::GroupLinkFailed`.
    JoinGroupLink { url: String },
    /// Fetches a group as the server has it now, as Desktop does when a
    /// group's chat or details open (`fetchLatestGroupV2Data`). At most
    /// every few minutes per group.
    RefreshGroup { conversation: ConversationId },
    /// Adds people (ACIs) to a group, inviting those whose profile key we
    /// don't have (Desktop's "Add members").
    AddGroupMembers { conversation: ConversationId, members: Vec<String> },
    /// Ends a group for everyone (an admin's "End group"), behind the remote
    /// config's `groupTerminate.send`.
    EndGroup { conversation: ConversationId },
    /// Sets (or with an empty `text`, clears) our label in a group.
    SetMemberLabel { conversation: ConversationId, emoji: Option<String>, text: String },
    /// Computes a 1:1 chat's safety number; answered with `Event::SafetyNumber`.
    LoadSafetyNumber { conversation: ConversationId },
    /// Marks a 1:1 contact's current identity verified (or clears it), here
    /// and on our other devices; answered with `Event::SafetyNumber`.
    SetVerified { conversation: ConversationId, verified: bool },
    /// Desktop's "Verify automatically" in their safety number screen: a
    /// key transparency check of them (and of us, until that has run once).
    /// Answered with `Event::KeyTransparency`.
    CheckKeyTransparency { conversation: ConversationId },
    /// The safety number screen closed: its key transparency check stops.
    CancelKeyTransparency,
    /// Automatic key verification off (`true`) or on, for the whole account
    /// (`AccountRecord.automaticKeyVerificationDisabled`). Off forgets what
    /// was verified.
    SetKeyTransparencyDisabled(bool),
    /// A megaphone's button: done with it (donate, finish) or put off
    /// (snooze). Answered with `Event::Megaphones`.
    PressMegaphone { id: String, cta: megaphones::MegaphoneCta },
    /// A story was shown: viewed here, on our other devices, and (with
    /// view receipts on) to its author.
    ViewStory { id: i64 },
    /// Downloads a story's photo or video.
    FetchStory { id: i64 },
    /// Replies to someone's story (or reacts, with `emoji`): one to one in
    /// your chat with them; a group's, with the story.
    ReplyToStory { id: i64, body: String, emoji: Option<String> },
    /// Posts a story to each of `targets`.
    PostStory { story: OutgoingStory, targets: Vec<StoryTarget> },
    /// Saves a story list (My Story's privacy, or a custom story), here and
    /// in Storage Service; a new id adds one.
    SaveStoryList(StoryList),
    /// Deletes a custom story list, and the stories sent to it for
    /// everyone (Desktop's `deleteDistributionList`).
    DeleteStoryList { id: String },
    /// Turns stories off (deleting the ones we sent, for everyone) or on,
    /// for the whole account (`AccountRecord.storiesDisabled`).
    SetStoriesDisabled(bool),
    /// Story view receipts on or off, for the whole account
    /// (`AccountRecord.storyViewReceiptsEnabled`).
    SetStoryViewReceipts(bool),
    /// Read receipts, typing indicators and the like on or off, for the
    /// whole account (Desktop's Privacy settings, kept in the
    /// `AccountRecord`).
    SetAccountSetting { setting: AccountSetting, on: bool },
    /// Adds groups' stories to the list we post to (`Always`, Desktop's
    /// "New group story") or removes them (`Never`, "Remove group story"),
    /// for the whole account (`GroupV2Record.storySendMode`).
    SetStorySendMode { groups: Vec<ConversationId>, mode: signal_store::StorySendMode },
    /// Deletes one of our stories for everyone who got it (Desktop's
    /// `deleteStoryForEveryone`).
    DeleteStory { id: i64 },
    /// Deletes a reply to a group story: here only, or (ours) for everyone
    /// in the group (Desktop's "Delete for me" / "Delete for everyone").
    DeleteStoryReply { story: i64, author: Option<String>, sent_at: i64, for_everyone: bool },
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
    /// For a call link's call (`ConversationId::call_link`) too, which
    /// also refreshes the link's name and settings.
    PeekGroupCall { conversation: ConversationId },
    /// Makes a call link (Desktop's "Create a call link"): answered with
    /// `Event::CallLinkCreated`, once the calling server has it.
    CreateCallLink,
    /// Renames a link of ours, or changes whether an admin approves who
    /// joins (Desktop's call link details).
    UpdateCallLink { room_id: String, name: Option<String>, restrictions: Option<signal_store::CallLinkRestrictions> },
    /// Deletes a link of ours for everyone, or forgets one we only joined.
    DeleteCallLink { room_id: String },
    /// A `https://signal.link/call/#key=…` URL: keeps the link and reads
    /// its name; answered with `Event::CallLinkOpened` (or an error).
    OpenCallLink { url: String },
    /// Lets someone waiting into our call link's call (their ACI), or not.
    ApproveCallJoin { aci: String, approve: bool },
    /// Takes someone out of our call link's call.
    RemoveFromCall { demux_id: u32 },
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
    /// Sends a reaction (an emoji) to everyone in the group call.
    CallReact { emoji: String },
    /// Raises (or lowers) our hand in the group call.
    RaiseHand(bool),
    /// Asks a participant's device to mute its microphone (Desktop's
    /// `sendRemoteMute`, behind the `remoteMute.send` flag; always on).
    RemoteMute { demux_id: u32 },
    /// The group call's participants on screen, whose video to receive
    /// (`main`: one shown large, at a higher resolution). Desktop's
    /// `setGroupCallVideoRequest`.
    ShowGroupVideo { visible: Vec<u32>, main: Option<u32> },
    /// Marks the call history read up to its latest call (`None`: every
    /// chat, as Desktop does when the Calls tab is viewed), and tells our
    /// other devices. Reading a chat does this for its calls.
    MarkCallsRead { conversation: Option<ConversationId> },
    /// Clears the call history (Desktop's "Clear call history"), here and
    /// on our other devices.
    ClearCallHistory,
    /// Unlinks this device and wipes local state.
    Unlink,
    /// Stops waiting for the history transfer and continues without it.
    /// Ignored once the import has started.
    SkipHistory,
    /// The window became active: fetches the remote config again if it's
    /// been two hours (Desktop's `maybeRefreshRemoteConfig`).
    RefreshRemoteConfig,
}

/// A per-chat setting kept in the Storage Service.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatChange {
    Archived(bool),
    /// Unix millis; [`MUTED_ALWAYS`] for always, 0 unmutes.
    MutedUntil(i64),
    MarkedUnread(bool),
    Blocked(bool),
    /// Their stories hidden (Desktop's Hide story).
    HideStory(bool),
}

/// A story list (Desktop's distribution list), from Storage Service.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct StoryList {
    /// Its UUID; My Story's is [`MY_STORY_ID`].
    pub id: String,
    pub name: String,
    /// Who it's shared with, or (`block_list`) who it isn't.
    pub members: Vec<String>,
    pub block_list: bool,
    pub allows_replies: bool,
}

impl StoryList {
    /// My Story as Desktop starts it: all Signal connections, replies on.
    pub fn my_story() -> Self {
        Self { id: MY_STORY_ID.into(), name: String::new(), members: Vec::new(), block_list: true, allows_replies: true }
    }

    pub fn is_my_story(&self) -> bool {
        self.id == MY_STORY_ID
    }

    /// A new custom story (Desktop's `createDistributionList`): replies on.
    pub fn new_custom(name: String, members: Vec<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), name, members, block_list: false, allows_replies: true }
    }
}

/// Desktop's `MY_STORY_ID`.
pub const MY_STORY_ID: &str = "00000000-0000-0000-0000-000000000000";
/// Settings key: the story lists (JSON [`StoryList`]s), from Storage Service.
pub const SETTING_STORY_LISTS: &str = "story_lists";
/// Settings key: `"true"` once My Story's viewers were chosen (the
/// account's `hasSetMyStoriesPrivacy`); until then the first post asks.
pub const SETTING_MY_STORY_PRIVACY_SET: &str = "my_story_privacy_set";
/// Settings key: chats whose stories are hidden (JSON conversation ids).
pub const SETTING_HIDDEN_STORIES: &str = signal_store::HIDDEN_STORIES;

/// Whether the composer offers "Poll" in this chat (Desktop's
/// `CompositionArea`): groups, and 1:1 chats when the remote config's
/// `pollSend1to1` flag is on ([`RemoteConfig::poll_send_1to1`]). Received
/// polls show anywhere.
pub fn can_create_poll(kind: signal_store::ConversationKind) -> bool {
    kind == signal_store::ConversationKind::Group || remote_config().poll_send_1to1()
}

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

/// A story to post.
#[derive(Debug, Clone, PartialEq)]
pub enum OutgoingStory {
    /// A text story, with the link preview it shows (Desktop's
    /// `TextAttachment.preview`; its image is uploaded with it).
    Text { story: Box<signal_store::TextStory>, preview: Option<LinkPreviewDraft> },
    /// A photo, or an MP4 video of up to 30 seconds, with a caption and its
    /// formatting (byte spans over the caption).
    Media { bytes: std::sync::Arc<Vec<u8>>, content_type: String, caption: String, styles: Vec<signal_store::StyleSpan> },
}

/// Where a story goes (Desktop's `SendStoryModal`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoryTarget {
    /// My Story: whoever its privacy says.
    MyStory,
    /// A custom story list, by id.
    List(String),
    /// A group's story, to its members.
    Group(ConversationId),
}

/// A view-once message's media, opened: a copy only the viewer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedViewOnce {
    pub message_id: i64,
    /// The copy; the viewer deletes it when it closes.
    pub path: PathBuf,
    pub content_type: String,
    /// Pixel size, when known.
    pub size: Option<(u32, u32)>,
}

/// A photo or video to send view-once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutgoingViewOnce {
    Image(OutgoingImage),
    /// A video, sent as it is.
    Video(OutgoingFile),
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
    /// A group call's raised hands (demux ids, ours as `local_demux_id`),
    /// in the order they went up.
    pub raised_hands: Vec<u32>,
    /// Our demux id in a group call, once we're in.
    pub local_demux_id: Option<u32>,
    /// A call link's call: what's particular to one.
    pub link: Option<CallLinkCall>,
}

/// A reaction someone sent in a group call (Desktop's call reactions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallReaction {
    /// Their ACI (ours when `demux_id` is our `local_demux_id`).
    pub aci: String,
    pub demux_id: u32,
    pub emoji: String,
}

/// A call link's call, beyond what every group call has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallLinkCall {
    /// The link is ours: we let people in.
    pub admin: bool,
    /// Who's asking to join (their ACIs), in the order they asked.
    pub pending: Vec<String>,
    /// We're waiting for an admin to let us in.
    pub waiting: bool,
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
/// A call link's URL from its root key (`https://signal.link/call/#key=…`).
pub use signal_calls::link_url as call_link_url;

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
    /// Whether the screen offers automatic key verification.
    pub auto_verify: AutoVerify,
}

/// Desktop's key transparency section in the safety number screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVerify {
    /// Turned off (in Settings, or on another device): not shown.
    Off,
    /// We don't know their phone number (Desktop's `unavailable`).
    Unavailable,
    /// "Verify automatically" can be pressed.
    Available,
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

/// Whether `len` bytes are over Signal's attachment size limit
/// (`global.attachments.maxBytes`, see [`RemoteConfig::attachment_max_bytes`]).
pub fn too_big_to_send(len: usize) -> bool {
    len as u64 > remote_config().attachment_max_bytes()
}

/// A size limit as Desktop shows it (`getRenderDetailsForLimit` in
/// `ts/types/AttachmentSize.std.ts`): whole kB, MB or GB, in steps of 1024.
pub fn size_limit_label(bytes: u64) -> String {
    let units = ["kB", "MB", "GB"];
    let (mut limit, mut unit) = (bytes as f64, 0);
    limit /= 1024.;
    while limit >= 1024. && unit < units.len() - 1 {
        limit /= 1024.;
        unit += 1;
    }
    format!("{} {}", limit.trunc(), units[unit])
}

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
    /// Signal's megaphones to show now, highest priority first.
    Megaphones(Vec<megaphones::MegaphoneView>),
    /// Stories came, went, were viewed or replied to.
    StoriesChanged,
    /// Stories' settings changed (hidden stories, stories off, view
    /// receipts, story lists), here or from Storage Service.
    StorySettingsChanged,
    /// An [`AccountSetting`] changed, here or on another device.
    AccountSettingsChanged,
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
    /// Reactions sent in the group call, oldest first.
    CallReactions(Vec<CallReaction>),
    /// In the group call, `by` (an ACI) muted `target` (`None`: us; then
    /// our microphone is off).
    CallMuted { by: String, target: Option<String> },
    /// Call links (or their calls in the history) changed: reload them.
    CallLinksChanged,
    /// A link we made is ready (its room id).
    CallLinkCreated(String),
    /// A link's URL was opened and the link is kept (its room id): its
    /// lobby can open.
    CallLinkOpened(String),
    /// A call link's call ended without us: an admin turned down our
    /// request, or removed us (Desktop's notices for both).
    CallLinkRefused { removed: bool },
    /// A 1:1 chat's safety number, or why there isn't one.
    SafetyNumber { conversation: ConversationId, result: Result<SafetyNumber, String> },
    /// Answer to `Command::CheckKeyTransparency`: whether their key (and
    /// ours) verified.
    KeyTransparency { conversation: ConversationId, verified: bool },
    /// Our own account's key transparency check has started failing:
    /// Desktop's "Automatic Key Verification is currently unavailable"
    /// dialog.
    KeyTransparencyFailed,
    /// A group we created is ready to open.
    GroupCreated(ConversationId),
    /// What a group link leads to, for Desktop's join dialog.
    GroupLinkPreview(GroupLinkPreview),
    /// Open this group (joined, asked to join, or already in it), with
    /// Desktop's toast if there is one ("You're already in this group.").
    GroupOpened { conversation: ConversationId, toast: Option<String> },
    /// A group link couldn't be read or joined: Desktop's title and words.
    GroupLinkFailed { title: String, description: String },
    /// Our answer to a message request took (for Desktop's toasts).
    MessageRequestAnswered { conversation: ConversationId, answer: signal_store::MessageRequestResponse },
    /// Progress of the message history transfer after linking.
    /// Draining the server queue after starting: the server timestamps
    /// (unix millis) of the first queued message and the latest one
    /// processed. Sent a few times a second at most, and only until the first
    /// `Connection(Online)`.
    CatchingUp { first: i64, latest: i64 },
    History(HistoryProgress),
    /// A view-once message we opened: show it in the viewer, without saving
    /// or forwarding, and delete the copy when it closes.
    ViewOnceOpened(OpenedViewOnce),
    /// A new message from someone else arrived (for notifications).
    Incoming { conversation: ConversationId, message_id: i64, mentions_me: bool },
    /// Someone voted in our poll (`ended`: they ended theirs), for
    /// Desktop's notifications. `message_id` is the poll; `sender` their name.
    PollActivity { conversation: ConversationId, message_id: i64, sender: String, ended: bool },
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
    /// The remote config changed (see [`remote_config()`]): limits and
    /// features may have too.
    RemoteConfigChanged,
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
