//! Application model: mirrors backend state for the views.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, Context, Entity, EventEmitter, SharedString, Task};
use signal_backend::{Backend, Command, ConnectionState, Event, Url};
use signal_store::{Conversation, ConversationId};

use crate::emoji_usage::EmojiUsageCache;
use crate::loop_cache::{LoopCache, LoopState};

/// Desktop's navigation tabs (`NavTab`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavTab {
    #[default]
    Chats,
    Calls,
    Stories,
}

/// Settings key: the tabs are hidden (Desktop's `navTabsCollapsed`).
const NAV_COLLAPSED_SETTING: &str = "nav_tabs_collapsed";
const ADMIN_DELETE_EDUCATION_SETTING: &str = "admin_delete_education_seen";
/// Settings key: chats whose stories are hidden (JSON conversation ids).
const HIDDEN_STORIES_SETTING: &str = signal_backend::SETTING_HIDDEN_STORIES;

/// A person's or group's stories, for the list and the viewer.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryGroup {
    pub chat: ConversationId,
    pub title: String,
    /// Oldest first.
    pub stories: Vec<signal_store::Story>,
    pub hidden: bool,
}

impl StoryGroup {
    pub fn unread(&self) -> bool {
        self.stories.iter().any(|s| !s.read)
    }

    pub fn latest(&self) -> i64 {
        self.stories.last().map_or(0, |s| s.sent_at)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    /// Opening databases / checking registration.
    Loading,
    Link { url: Option<Url>, error: Option<String> },
    Main,
}

/// Catching up on the messages queued while the app was closed. Like
/// Desktop, the app waits on this (see `views::syncing`).
#[derive(Debug, Clone, PartialEq)]
pub struct CatchUp {
    /// When it started (unix millis): the bar runs from the first queued
    /// message's server time to this.
    pub since: i64,
    /// Server times of the first queued message and the latest one processed.
    pub first: Option<i64>,
    pub latest: Option<i64>,
}

impl CatchUp {
    /// 0–1, or `None` before the first message.
    pub fn fraction(&self) -> Option<f32> {
        let (first, latest) = (self.first?, self.latest?);
        let span = (self.since - first).max(1) as f32;
        Some(((latest - first).max(0) as f32 / span).clamp(0., 1.))
    }
}

/// How long to wait for the connection before showing the app anyway.
const CATCH_UP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a saved setting's value outlasts reloads that don't read it back.
const PENDING_PREF_TIMEOUT: Duration = Duration::from_secs(30);

/// Desktop's lobby: the camera preview and toggles before a call starts.
#[derive(Debug, Clone, PartialEq)]
pub struct CallLobby {
    pub conversation: ConversationId,
    pub video: bool,
    pub audio: bool,
    /// Who's in a group's call already (their ACIs); `None` until we know.
    pub peek: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub phone: Option<String>,
    pub aci: String,
}

/// Events for views; backend events are translated into these.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum AppEvent {
    ConversationsChanged,
    SelectionChanged,
    MessageChanged { conversation: ConversationId, message_id: i64 },
    MessageRemoved { conversation: ConversationId, message_id: i64 },
    /// Show this message (a search result) in its chat.
    /// (The message is taken with `AppState::take_jump`.)
    JumpTo { conversation: ConversationId },
    /// Many of a chat's messages went at once; reload it.
    ConversationCleared(ConversationId),
    PinsChanged(ConversationId),
    TypingChanged { conversation: ConversationId },
    /// `(service id, name)` pairs whose stored author names were rewritten.
    AuthorNamesChanged(std::rc::Rc<Vec<(String, String)>>),
    /// Something failed that the user should hear about.
    Error(SharedString),
    /// Answer to a username or phone number lookup.
    RecipientFound { query: String, result: Result<signal_store::DirectoryEntry, String> },
    /// GIF search results (`Err`: why not).
    Gifs { query: String, offset: u32, result: Result<signal_backend::GifResults, String> },
    /// Desktop's "Search in chat": the sidebar's search box searches this
    /// chat.
    SearchInChat(ConversationId),
    /// Show an image full-window.
    ViewImage(crate::views::image_viewer::ViewedImage),
    ViewVideo(crate::views::video_viewer::ViewedVideo),
    /// Open the story viewer at these stories (a chat's; `None`: ours).
    ViewStories(Option<ConversationId>),
    /// Open the story creator: a text story, or this photo or video
    /// (its bytes, type and file).
    CreateStory(Option<(std::sync::Arc<Vec<u8>>, String, std::path::PathBuf)>),
    /// A link preview the composer asked for (`None`: nothing to show).
    LinkPreview { url: String, preview: Option<std::rc::Rc<signal_backend::LinkPreviewDraft>> },
    /// How the sticker pack creator's upload is going.
    StickerUpload(signal_backend::sticker_upload::StickerUpload),
    /// Call links or their calls changed (`AppState::call_links` is reloaded).
    CallLinksChanged,
    /// A call link we made is ready: show its details (its room id).
    CallLinkCreated(String),
    /// Our own key transparency check started failing: Desktop's
    /// `KeyTransparencyErrorDialog`.
    KeyTransparencyFailed,
    /// A send needs a captcha first: Desktop's captcha dialog.
    CaptchaRequired,
    /// The captcha was answered (accepted or not): its dialog closes.
    CaptchaDone,
    /// The critical idle-phone alert's modal is due.
    CriticalAlertModal,
    /// Desktop's join dialog for a group link (`GroupV2JoinDialog`).
    GroupLinkPreview(std::rc::Rc<signal_backend::GroupLinkPreview>),
    /// A sticker pack is opening (`AppState::open_sticker_pack`): show
    /// Desktop's `StickerPreviewModal`.
    OpenStickerPack,
    /// Desktop's error dialog: a title and what went wrong.
    Alert { title: SharedString, description: SharedString },
    /// A short note to show (Desktop's toasts).
    Info(SharedString),
    /// Someone's profile (name, about, badges) or contact details changed.
    ProfileChanged(String),
    /// Our profile, username or link changed (`AppState::own_profile`).
    OwnProfileChanged,
    /// Answer to writing our profile.
    ProfileSaved(Result<(), String>),
    /// Answer to reserving a username, for that nickname and digits.
    UsernameReserved {
        nickname: String,
        discriminator: Option<String>,
        result: Result<signal_backend::usernames::Reservation, signal_backend::usernames::UsernameProblem>,
    },
    /// Answer to confirming (`Some`) or deleting (`None`) our username.
    UsernameSaved(Result<Option<String>, signal_backend::usernames::UsernameProblem>),
    /// Answer to resetting our username link.
    UsernameLinkReset(Result<(), String>),
}

/// Badges: who shows which, their descriptions, and their downloaded
/// images (Desktop's `badges` state).
#[derive(Debug, Clone, Default)]
pub struct Badges {
    /// Each person's visible badges' ids, the first one first.
    pub holders: HashMap<String, Vec<String>>,
    pub badges: HashMap<String, signal_store::Badge>,
    /// Downloaded images by URL.
    pub files: HashMap<String, std::path::PathBuf>,
}

impl Badges {
    /// The badge to draw on someone's avatar (Desktop's `badges[0]`).
    pub fn preferred(&self, service_id: &str) -> Option<&signal_store::Badge> {
        self.holders.get(service_id)?.iter().find_map(|id| self.badges.get(id))
    }

    /// All of their visible badges, for the badge dialog.
    pub fn of(&self, service_id: &str) -> Vec<signal_store::Badge> {
        self.holders.get(service_id).into_iter().flatten().filter_map(|id| self.badges.get(id).cloned()).collect()
    }

    /// The downloaded image of `badge` to draw at `size` pixels.
    pub fn image(&self, badge: &signal_store::Badge, size: f32, dark: bool) -> Option<std::path::PathBuf> {
        let theme = if dark { signal_store::BadgeTheme::Dark } else { signal_store::BadgeTheme::Light };
        let url = badge.image_for(size, theme, |url| self.files.contains_key(url))?;
        self.files.get(&url).cloned()
    }
}

/// The key transparency button in the safety number screen, in Desktop's
/// states (`KeyTransparencyStatusType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KtStatus {
    Idle,
    Running,
    Ok,
    Fail,
    /// We don't know their phone number.
    Unavailable,
}

/// The audio attachment being played.
pub struct AudioPlaying {
    pub message_id: i64,
    pub index: usize,
    /// Who sent it ("You" for ours), for the mini player.
    pub title: String,
    player: Box<dyn crate::audio::Playback>,
    pub rate: f32,
    /// When it ends, the chat's next audio plays (Desktop's consecutive
    /// playback; not from All media).
    consecutive: bool,
    /// Where it was when last seen playing: near the end, a stop is the end.
    last_position: f64,
    /// Redraws while it plays, and notices when it ends.
    _poll: Task<()>,
}

impl AudioPlaying {
    pub fn is_playing(&self) -> bool {
        self.player.is_playing()
    }

    /// Where it is, and how long it is, in seconds.
    pub fn progress(&self) -> (f64, f64) {
        (self.player.position(), self.player.duration())
    }
}

/// What plays next from a message: its first audio attachment, if it's
/// downloaded, with who sent it for the mini player.
fn next_playable(message: &signal_store::Message) -> Option<(i64, usize, std::path::PathBuf, String)> {
    let (index, attachment) = message.attachments.iter().enumerate().find(|(_, a)| a.content_type.starts_with("audio/"))?;
    let title = match message.direction {
        signal_store::Direction::Outgoing => "You".to_owned(),
        signal_store::Direction::Incoming => message.author_name.clone().unwrap_or_else(|| "Unknown".into()),
    };
    Some((message.id, index, std::path::PathBuf::from(attachment.local_path.clone()?), title))
}

/// An audio file's waveform peaks (0 to 1) and its length in seconds.
pub type Waveform = (std::rc::Rc<Vec<f32>>, f64);

/// How often the audio player redraws its progress.
const AUDIO_POLL: Duration = Duration::from_millis(50);
/// Seen within this many seconds of the end, then stopped and rewound: it
/// finished (rather than being paused at the start).
const END_SLACK: f64 = 0.3;

pub struct AppState {
    pub backend: Backend,
    pub screen: Screen,
    pub account: Option<Account>,
    pub connection: ConnectionState,
    pub conversations: Vec<Conversation>,
    pub archived: Vec<Conversation>,
    /// A message to show when its chat's view opens (from search).
    jump: Option<(ConversationId, i64)>,
    /// Downloaded profile photos by service id.
    person_avatars: HashMap<String, std::path::PathBuf>,
    /// People whose safety number changed after we verified it: sends to
    /// them ask first (`signal_backend::unverified_people`).
    pub unverified: std::collections::HashSet<String>,
    /// A captcha is being solved in the browser (its dialog's Continue
    /// spins), Desktop's challenge status `pending`.
    pub captcha_pending: bool,
    /// The server's alerts as last connected.
    pub server_alerts: signal_backend::server_alerts::ServerAlerts,
    /// Our clock is too far from the server's.
    pub clock_skew: bool,
    /// Our profile, username and link (`None` until read).
    pub own_profile: Option<signal_backend::OwnProfile>,
    pub badges: Badges,
    /// Photos from the phone's address book by service id (the contact
    /// sync's), for Desktop's "Use address book photos".
    contact_avatars: HashMap<String, std::path::PathBuf>,
    /// Desktop's "Import now", while it runs or once it failed.
    pub contact_sync: ContactSync,
    pub selected: Option<ConversationId>,
    /// Authors currently typing, per conversation, with expiry.
    typing: HashMap<ConversationId, Vec<(String, Instant)>>,
    pub emoji_usage: EmojiUsageCache,
    pub last_error: Option<String>,
    /// Installed sticker packs (refreshed when the picker opens).
    pub sticker_packs: Vec<std::sync::Arc<signal_backend::StickerPackView>>,
    /// Chat colors: the default, custom colors, and each chat's own.
    pub chat_colors: signal_store::ChatColors,
    /// What Desktop's sticker manager lists, once loaded.
    pub sticker_manager: Option<signal_backend::stickers::StickerManagerView>,
    /// The sticker pack open in Desktop's `StickerPreviewModal`.
    pub sticker_preview: Option<StickerPreview>,
    /// Chat folders, deleted ones too (see [`Self::current_folders`]).
    pub chat_folders: Vec<signal_store::ChatFolder>,
    /// The folder the chat list shows (Desktop's `selectedChatFolderId`);
    /// `None` is "All chats".
    pub selected_folder: Option<String>,
    /// Whether a Giphy API key is saved (GIF search needs one).
    pub has_giphy_key: bool,
    /// Decoded looping videos (GIFs), by file and width.
    loops: LoopCache<std::sync::Arc<gpui_kit::RenderImage>>,
    /// The audio attachment or voice message playing (or paused), as
    /// Desktop plays one at a time, whichever chat is open.
    pub audio: Option<AudioPlaying>,
    /// Opens audio to play (a silent stand-in in tests).
    pub open_audio: crate::audio::Opener,
    /// Signal's megaphones to show, highest priority first.
    pub megaphones: Vec<signal_backend::megaphones::MegaphoneView>,
    /// The navigation tab showing.
    pub tab: NavTab,
    /// The tabs are hidden.
    pub nav_collapsed: bool,
    /// The stories up now, newest first.
    pub stories: Vec<signal_store::Story>,
    /// Chats (people or groups) whose stories are hidden.
    pub hidden_stories: std::collections::HashSet<ConversationId>,
    /// Stories are turned off.
    pub stories_disabled: bool,
    /// Desktop's admin delete confirmation has been confirmed once
    /// (`hasSeenAdminDeleteEducationDialog`), so it isn't shown again.
    pub seen_admin_delete_education: bool,
    /// Story view receipts, if set (unset follows read receipts).
    pub story_view_receipts: Option<bool>,
    /// Read receipts are on (what unset story view receipts follow).
    pub read_receipts: bool,
    /// Typing indicators are on: ours sent, others' shown.
    pub typing_indicators: bool,
    /// The story lists: My Story's privacy and custom stories.
    pub story_lists: Vec<signal_backend::StoryList>,
    /// My Story's viewers were chosen (Desktop's `hasSetMyStoriesPrivacy`);
    /// until then, choosing My Story asks.
    pub my_story_privacy_set: bool,
    /// Groups' story send modes that aren't the default (Desktop's
    /// `storySendMode`).
    pub story_send_modes: HashMap<ConversationId, signal_store::StorySendMode>,
    /// Starts recording a voice message (the microphone; a stand-in in
    /// tests).
    pub start_recorder: crate::recorder::StartRecorder,
    /// Audio files' waveforms and lengths (`None` while decoding, or if it
    /// can't be).
    waveforms: HashMap<std::path::PathBuf, Option<Waveform>>,
    /// Video attachments' first frames (`None` while being made, or if
    /// they can't be).
    posters: HashMap<std::path::PathBuf, Option<std::path::PathBuf>>,
    /// Recently sent stickers, newest first: (pack id, sticker id).
    pub recent_stickers: Vec<(Vec<u8>, u32)>,
    /// How images are optimized before sending (our setting, not Signal's).
    pub image_optimization: signal_backend::media::Optimization,
    /// Settings' other values (theme, zoom, the account's on/off settings).
    pub prefs: crate::prefs::Prefs,
    /// Settings' values being saved, and since when (see `save_pref`).
    pending_prefs: HashMap<&'static str, (String, std::time::Instant)>,
    /// The newest `load_prefs` read; an older one finishing late is dropped.
    prefs_read: u64,
    /// Open at computer login, as the system last said.
    pub open_at_login: bool,
    /// The system's login item (a stand-in in tests).
    pub login_item: crate::login_item::LoginItem,
    reload_task: Option<Task<()>>,
    reload_pending: bool,
    /// Demo mode opens the first conversation once loaded.
    auto_select: bool,
    /// System notifications (`None` where they can't be shown).
    notifier: Option<std::rc::Rc<crate::notifications::Notifier>>,
    /// Notifications on screen per chat, taken down once it's read.
    shown_notifications: HashMap<ConversationId, Vec<String>>,
    pub notification_setting: crate::notifications::NotificationSetting,
    /// The main window is in front (no notifications for the open chat).
    pub window_active: bool,
    /// The message history transfer after linking, while it runs.
    pub history: Option<signal_backend::HistoryProgress>,
    /// The call in progress (Desktop's call screen or incoming call bar).
    pub call: Option<signal_backend::CallUpdate>,
    /// Desktop's call lobby, before calling.
    pub lobby: Option<CallLobby>,
    /// Call links we made or used, newest first.
    pub call_links: Vec<signal_store::CallLink>,
    /// Our microphone and camera in the call.
    pub call_audio: bool,
    pub call_video: bool,
    /// The call screen is minimized to a small window.
    pub call_minimized: bool,
    /// A group call's recent reactions, with when each came (Desktop shows
    /// each for a few seconds).
    pub call_reactions: Vec<(std::time::Instant, signal_backend::CallReaction)>,
    /// The group call's latest mute: when, who did it, and whom (`None`:
    /// us), for Desktop's mute toast.
    pub call_mute: Option<(std::time::Instant, String, Option<String>)>,
    /// Screens and windows to present (loaded for the picker).
    pub share_sources: Option<Result<Vec<signal_backend::ShareSource>, String>>,
    /// Microphones, speakers and cameras, for Settings (loaded on opening it).
    pub call_devices: Option<signal_backend::CallDevices>,
    /// Ringing for an incoming call.
    ringtone: Option<crate::ringtone::Ringtone>,
    /// The safety number last loaded (`Command::LoadSafetyNumber`).
    pub safety_number: Option<(ConversationId, Result<signal_backend::SafetyNumber, String>)>,
    /// The safety number screen's key transparency check, once started.
    pub kt_status: Option<(ConversationId, KtStatus)>,
    /// Automatic key verification is off (Settings, Privacy).
    pub key_transparency_disabled: bool,
    /// Desktop's key transparency introduction was seen.
    pub kt_onboarding_seen: bool,
    pub catching_up: Option<CatchUp>,
    /// Like Desktop, notifications wait until the catch-up is done, and then
    /// only the latest is shown.
    held_notification: Option<(ConversationId, i64, Mentions)>,
    /// Reaction notifications on screen per chat, taken down once it's
    /// opened (they don't make it unread).
    reaction_notifications: HashMap<ConversationId, Vec<String>>,
    /// Checks for unread reminders hourly while an account is linked.
    reminder_task: Option<Task<()>>,
    /// The `signal.me` link being looked up (`open_signal_link`); its
    /// answer opens the chat.
    link_lookup: Option<String>,
    /// Links macOS handed us while the account was still loading (Desktop's
    /// `macInitialOpenUrlRoute`), opened once it's known.
    pending_links: Vec<String>,
}

impl EventEmitter<AppEvent> for AppState {}

/// How long "Import now" waits for the phone's contacts before saying it
/// failed.
const CONTACT_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// Desktop's "Import now" in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactSync {
    Idle,
    /// Asked for, since then.
    Pending(Instant),
    /// They didn't come in time (Desktop's `syncFailed`).
    Failed,
}

/// The chat due an unread reminder, and its unread messages.
pub type PickedReminder =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<(ConversationId, signal_store::ReminderData)>> + Send>>;

/// What a new message says about us, for notifications in muted chats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mentions {
    pub mentions_me: bool,
    pub replies_to_me: bool,
}

/// Desktop's `isAllowedByConversation` for a muted chat: never a 1:1
/// chat; in a group, a mention or a reply to us when those notify while
/// muted (on by default).
pub(crate) fn allowed_while_muted(direct: bool, mentions: Mentions, prefs: &crate::prefs::Prefs) -> bool {
    use signal_backend::AccountSetting;
    !direct
        && ((mentions.mentions_me && prefs.flag(AccountSetting::NotifyMentionsIfMuted))
            || (mentions.replies_to_me && prefs.flag(AccountSetting::NotifyRepliesIfMuted)))
}

/// Desktop's `getConversationInDirection`
/// (ts/components/leftPane/getConversationInDirection.dom.ts), over the
/// list's chats by whether each is unread: with nothing open, the first
/// (or last) one; unread only, the nearest unread one that way (none past
/// the end); otherwise the next one, wrapping around.
pub(crate) fn conversation_in_direction(unread: &[bool], selected: Option<usize>, up: bool, unread_only: bool) -> Option<usize> {
    let count = unread.len();
    if count == 0 {
        return None;
    }
    match (selected.filter(|&ix| ix < count), unread_only) {
        (None, true) if up => unread.iter().rposition(|&u| u),
        (None, true) => unread.iter().position(|&u| u),
        (None, false) => Some(if up { count - 1 } else { 0 }),
        (Some(ix), true) if up => unread[..ix].iter().rposition(|&u| u),
        (Some(ix), true) => unread[ix + 1..].iter().position(|&u| u).map(|found| ix + 1 + found),
        (Some(ix), false) if up => Some(if ix == 0 { count - 1 } else { ix - 1 }),
        (Some(ix), false) => Some(if ix + 1 >= count { 0 } else { ix + 1 }),
    }
}

/// Desktop's `notificationReactionMessage`: "{sender} reacted {emoji} to:
/// {message}".
pub(crate) fn reaction_text(sender: &str, emoji: &str, message: &str) -> String {
    format!("{sender} reacted {emoji} to: {message}")
}

/// Typing indicators expire if no "stopped" arrives (Signal uses 15s).
const TYPING_TIMEOUT: Duration = Duration::from_secs(15);

impl AppState {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            screen: Screen::Loading,
            account: None,
            connection: ConnectionState::Offline,
            conversations: Vec::new(),
            archived: Vec::new(),
            person_avatars: HashMap::new(),
            unverified: Default::default(),
            captcha_pending: false,
            server_alerts: Default::default(),
            clock_skew: false,
            own_profile: None,
            badges: Badges::default(),
            contact_avatars: HashMap::new(),
            contact_sync: ContactSync::Idle,
            jump: None,
            selected: None,
            typing: HashMap::new(),
            emoji_usage: EmojiUsageCache::default(),
            last_error: None,
            image_optimization: Default::default(),
            prefs: Default::default(),
            open_at_login: false,
            pending_prefs: HashMap::new(),
            prefs_read: 0,
            login_item: Default::default(),
            sticker_packs: Vec::new(),
            chat_colors: Default::default(),
            chat_folders: Vec::new(),
            selected_folder: None,
            sticker_manager: None,
            sticker_preview: None,
            has_giphy_key: std::env::var_os("GIPHY_API_KEY").is_some(),
            loops: Default::default(),
            audio: None,
            open_audio: crate::audio::open,
            start_recorder: crate::recorder::start_microphone,
            megaphones: Vec::new(),
            tab: NavTab::default(),
            nav_collapsed: false,
            stories: Vec::new(),
            hidden_stories: Default::default(),
            stories_disabled: false,
            seen_admin_delete_education: false,
            story_view_receipts: None,
            read_receipts: false,
            typing_indicators: false,
            story_lists: Vec::new(),
            my_story_privacy_set: false,
            story_send_modes: HashMap::new(),
            waveforms: HashMap::new(),
            posters: HashMap::new(),
            recent_stickers: Vec::new(),
            reload_task: None,
            reload_pending: false,
            auto_select: std::env::var_os("SIGNAL_GPUI_DEMO").is_some(),
            notifier: None,
            shown_notifications: HashMap::new(),
            notification_setting: Default::default(),
            window_active: true,
            history: None,
            safety_number: None,
            kt_status: None,
            key_transparency_disabled: false,
            kt_onboarding_seen: false,
            call: None,
            lobby: None,
            call_links: Vec::new(),
            call_audio: true,
            call_video: false,
            call_minimized: false,
            call_reactions: Vec::new(),
            call_mute: None,
            ringtone: None,
            call_devices: None,
            share_sources: None,
            catching_up: None,
            held_notification: None,
            reaction_notifications: HashMap::new(),
            reminder_task: None,
            link_lookup: None,
            pending_links: Vec::new(),
        };
        this.load_emoji_usage(cx);
        this.start_notifications(cx);
        this
    }

    fn start_notifications(&mut self, cx: &mut Context<Self>) {
        use futures::StreamExt as _;
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        self.notifier = crate::notifications::Notifier::start(tx).map(std::rc::Rc::new);
        if self.notifier.is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            while let Some(action) = rx.next().await {
                if this.update(cx, |this, cx| this.on_notification(action, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Something done in a notification: open the chat, send a reply, or
    /// mark it read.
    fn on_notification(&mut self, action: crate::notifications::NotificationAction, cx: &mut Context<Self>) {
        use crate::notifications::NotificationAction;
        match action {
            NotificationAction::Open(conversation) => {
                cx.activate(true);
                self.select(Some(conversation), cx);
            }
            NotificationAction::Reply(conversation, text) => {
                // Formatting markers work here as in the composer.
                let (body, styles) = crate::formatting::parse(&text);
                self.send(Command::SendText {
                    conversation: conversation.clone(),
                    body,
                    quote: None,
                    preview: None,
                    mentions: Vec::new(),
                    styles,
                });
                self.send(Command::MarkRead { conversation });
            }
            NotificationAction::MarkRead(conversation) => self.send(Command::MarkRead { conversation }),
        }
    }

    /// Notifies about a new message unless you're looking at its chat, it's
    /// blocked, or it's muted (in a group, a mention or a reply to us still
    /// notifies when those notify while muted, as Desktop's
    /// `isAllowedByConversation`). In the open chat, Desktop's in-chat
    /// sound plays instead, if on.
    fn maybe_notify(&mut self, conversation: ConversationId, message_id: i64, mentions: Mentions, cx: &mut Context<Self>) {
        if self.catching_up.is_some() {
            self.held_notification = Some((conversation, message_id, mentions));
            return;
        }
        if self.window_active && self.selected.as_ref() == Some(&conversation) {
            if self.tab == NavTab::Chats && self.prefs.on(crate::prefs::Toggle::MessageSounds) {
                crate::sounds::play(crate::sounds::Sound::Pop);
            }
            return;
        }
        let setting = self.notification_setting;
        if self.notifier.is_none() || setting == crate::notifications::NotificationSetting::Off {
            return;
        }
        let prefs = self.prefs.clone();
        let db = self.backend.db().clone();
        let id = conversation.clone();
        let query = self.backend.spawn(async move {
            Ok::<_, signal_store::Error>((db.conversation(&id).await?, db.message(message_id).await?))
        });
        cx.spawn(async move |this, cx| {
            let Ok((Some(chat), Some(message))) = query.await else { return };
            let direct = chat.kind != signal_store::ConversationKind::Group;
            if chat.blocked || (chat.muted && !allowed_while_muted(direct, mentions, &prefs)) {
                return;
            }
            let sound = prefs.on(crate::prefs::Toggle::NotificationSounds);
            let _ = this.update(cx, |this, _| {
                let Some(notifier) = this.notifier.clone() else { return };
                let sender = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
                let group = (chat.kind == signal_store::ConversationKind::Group).then_some(chat.title.as_str());
                let sender = if group.is_some() { sender } else { chat.title.clone() };
                // Desktop shows a request's notifications without what it
                // says (`getNotificationTextForMessage`).
                let preview = if chat.accepted { notification_preview(&message) } else { "Message Request".into() };
                let (title, body) = setting.text(&sender, group, &preview);
                let id = format!("{}:{message_id}", chat.id.0);
                notifier.post(&crate::notifications::Notification {
                    id: &id,
                    conversation: &chat.id,
                    title: &title,
                    subtitle: None,
                    body: &body,
                    sound,
                });
                this.shown_notifications.entry(chat.id.clone()).or_default().push(id);
            });
        })
        .detach();
    }

    /// Desktop's poll notifications (`maybeNotify`): someone voted in our
    /// poll ("Alice voted in the poll "📊 Poll: …""), or ended theirs.
    /// They follow the same rules as messages' (setting, muted chats, the
    /// chat in front), except while catching up, when they're dropped.
    /// Both are unread until the chat is read, which takes them down.
    fn notify_poll(&mut self, conversation: ConversationId, message_id: i64, sender: String, ended: bool, cx: &mut Context<Self>) {
        let setting = self.notification_setting;
        if self.catching_up.is_some() || self.notifier.is_none() || setting == crate::notifications::NotificationSetting::Off {
            return;
        }
        if self.window_active && self.selected.as_ref() == Some(&conversation) {
            return;
        }
        let db = self.backend.db().clone();
        let id = conversation.clone();
        let query = self.backend.spawn(async move {
            Ok::<_, signal_store::Error>((db.conversation(&id).await?, db.message(message_id).await?))
        });
        cx.spawn(async move |this, cx| {
            let Ok((Some(chat), Some(message))) = query.await else { return };
            let Some(poll) = message.poll.as_ref() else { return };
            if chat.blocked || chat.muted {
                return;
            }
            let _ = this.update(cx, |this, _| {
                let Some(notifier) = this.notifier.clone() else { return };
                let group = (chat.kind == signal_store::ConversationKind::Group).then_some(chat.title.as_str());
                let sender = if group.is_some() { sender.clone() } else { chat.title.clone() };
                let preview = poll_activity_text(&sender, group, &poll.question, ended);
                let (title, body) = setting.text(&sender, group, &preview);
                let id = format!("{}:{message_id}:poll-{}", chat.id.0, if ended { "ended" } else { "vote" });
                notifier.post(&crate::notifications::Notification {
                    id: &id,
                    conversation: &chat.id,
                    title: &title,
                    subtitle: None,
                    body: &body,
                    sound: this.prefs.on(crate::prefs::Toggle::NotificationSounds),
                });
                this.shown_notifications.entry(chat.id.clone()).or_default().push(id);
            });
        })
        .detach();
    }

    /// Desktop's reaction notifications (`maybeNotify`, kind `reaction`):
    /// someone else reacted to one of our messages ("Alice reacted 👍 to:
    /// …"), with "Reaction notifications" on, under the same rules as a
    /// message's (dropped while catching up, as poll votes are). A reaction
    /// taken back takes its notification down.
    fn notify_reaction(
        &mut self,
        conversation: ConversationId,
        message_id: i64,
        author: String,
        sender: String,
        emoji: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let id = format!("{}:{message_id}:react-{author}", conversation.0);
        let Some(emoji) = emoji else {
            if let Some(ids) = self.reaction_notifications.get_mut(&conversation)
                && let Some(ix) = ids.iter().position(|shown| *shown == id)
            {
                ids.remove(ix);
                if let Some(notifier) = &self.notifier {
                    notifier.remove(&[id]);
                }
            }
            return;
        };
        let setting = self.notification_setting;
        if self.catching_up.is_some()
            || self.notifier.is_none()
            || setting == crate::notifications::NotificationSetting::Off
            || !self.prefs.flag(signal_backend::AccountSetting::ReactionNotifications)
        {
            return;
        }
        if self.window_active && self.selected.as_ref() == Some(&conversation) {
            return;
        }
        let db = self.backend.db().clone();
        let chat_id = conversation.clone();
        let query = self.backend.spawn(async move {
            Ok::<_, signal_store::Error>((db.conversation(&chat_id).await?, db.message(message_id).await?))
        });
        cx.spawn(async move |this, cx| {
            let Ok((Some(chat), Some(message))) = query.await else { return };
            // Muted: Desktop lets only mentions and replies through.
            if chat.blocked || chat.muted {
                return;
            }
            let _ = this.update(cx, |this, _| {
                let Some(notifier) = this.notifier.clone() else { return };
                let group = (chat.kind == signal_store::ConversationKind::Group).then_some(chat.title.as_str());
                let sender = if group.is_some() { sender.clone() } else { chat.title.clone() };
                let preview = reaction_text(&sender, &emoji, &notification_preview(&message));
                let (title, body) = setting.text(&sender, group, &preview);
                notifier.post(&crate::notifications::Notification {
                    id: &id,
                    conversation: &chat.id,
                    title: &title,
                    subtitle: None,
                    body: &body,
                    sound: this.prefs.on(crate::prefs::Toggle::NotificationSounds),
                });
                let shown = this.reaction_notifications.entry(chat.id.clone()).or_default();
                if !shown.contains(&id) {
                    shown.push(id);
                }
            });
        })
        .detach();
    }

    /// Desktop's unread reminder service: a first check a minute in, then
    /// hourly (`UNREAD_REMINDER_POLL_INTERVAL`).
    fn start_unread_reminders(&mut self, cx: &mut Context<Self>) {
        self.reminder_task = Some(cx.spawn(async move |this, cx| {
            let mut wait = std::time::Duration::from_secs(60);
            loop {
                cx.background_executor().timer(wait).await;
                wait = crate::reminders::POLL_INTERVAL;
                if this.update(cx, |this, cx| this.check_unread_reminders(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Desktop's `#doRefresh`: with notifications and unread reminders on,
    /// reminds of one muted chat's unread messages, if one is due.
    fn check_unread_reminders(&mut self, cx: &mut Context<Self>) {
        let setting = self.notification_setting;
        if self.notifier.is_none() || setting == crate::notifications::NotificationSetting::Off || self.catching_up.is_some() {
            return;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let pick = self.pick_unread_reminder(now);
        cx.spawn(async move |this, cx| {
            let Some((chat, data)) = pick.await else { return };
            let _ = this.update(cx, |this, _| this.post_unread_reminder(chat, data, now));
        })
        .detach();
    }

    /// Desktop's `#isEligible`, `filterForConversationsDueReminder` and
    /// `#getSummary`: the muted, unarchived chat (not Note to Self, not
    /// blocked or left) whose unread messages are due a reminder, and what
    /// they are. Also marks when reminders started, the first time.
    pub fn pick_unread_reminder(&self, now: i64) -> PickedReminder {
        use crate::reminders::{ENABLED_AT_KEY, MIN_INTERVAL_MS, REMINDED_KEY, Reminded};
        let on = self.prefs.flag(signal_backend::AccountSetting::ShowUnreadReminders);
        let eligible: Vec<ConversationId> = self
            .conversations
            .iter()
            .filter(|c| {
                c.kind != signal_store::ConversationKind::NoteToSelf
                    && !c.id.is_signal()
                    && !c.left
                    && !c.blocked
                    && !c.archived
                    && c.muted
                    && c.unread_count > 0
            })
            .map(|c| c.id.clone())
            .collect();
        let me = self.account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
        let db = self.backend.db().clone();
        let query = self.backend.spawn(async move {
            let enabled_at = match db.setting(ENABLED_AT_KEY).await.ok().flatten().and_then(|v| v.parse::<i64>().ok()) {
                Some(at) => at,
                None => {
                    let _ = db.set_setting(ENABLED_AT_KEY, &now.to_string()).await;
                    now
                }
            };
            if !on || eligible.is_empty() {
                return None;
            }
            let reminded: Reminded =
                db.setting(REMINDED_KEY).await.ok().flatten().and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default();
            let chats: Vec<(ConversationId, i64)> = eligible
                .into_iter()
                .filter(|c| reminded.get(&c.0).is_none_or(|at| *at <= now - MIN_INTERVAL_MS))
                .map(|c| {
                    let since = reminded.get(&c.0).copied().unwrap_or(enabled_at);
                    (c, since)
                })
                .collect();
            let ranges = db.unreminded_unread_ranges(&chats).await.ok()?;
            for chat in crate::reminders::due(ranges, now) {
                if let Ok(data) = db.unread_reminder_data(&chat, &me).await
                    && data.unread > 0
                {
                    return Some((chat, data));
                }
            }
            None
        });
        Box::pin(query)
    }

    /// Shows an unread reminder and records it (Desktop's `#markReminded`).
    fn post_unread_reminder(&mut self, chat: ConversationId, data: signal_store::ReminderData, now: i64) {
        use crate::notifications::NotificationSetting;
        use signal_backend::AccountSetting;
        let Some(notifier) = self.notifier.clone() else { return };
        // Desktop's short titles: a first name.
        let name = |sid: &String| {
            self.conversation(&ConversationId::direct(sid))
                .and_then(|c| c.title.split_whitespace().next().map(str::to_owned))
                .unwrap_or_else(|| "Unknown contact".into())
        };
        let summary = crate::reminders::Summary {
            title: self.conversation(&chat).map(|c| c.title.clone()).unwrap_or_default(),
            unread: data.unread,
            mentions: if self.prefs.flag(AccountSetting::NotifyMentionsIfMuted) { data.mentions } else { 0 },
            replies: if self.prefs.flag(AccountSetting::NotifyRepliesIfMuted) { data.replies } else { 0 },
            senders: data.senders.iter().map(name).collect(),
            mentioners: data.mentioners.iter().map(name).collect(),
            repliers: data.repliers.iter().map(name).collect(),
        };
        let full = matches!(self.notification_setting, NotificationSetting::NameAndMessage | NotificationSetting::NameOnly);
        let (title, body) = crate::reminders::content(&summary, full);
        let id = format!("{}:unread-reminder", chat.0);
        notifier.post(&crate::notifications::Notification {
            id: &id,
            conversation: &chat,
            title: &title,
            subtitle: None,
            body: &body,
            sound: self.prefs.on(crate::prefs::Toggle::NotificationSounds),
        });
        self.shown_notifications.entry(chat.clone()).or_default().push(id);
        let db = self.backend.db().clone();
        let save = self.backend.spawn(async move {
            use crate::reminders::{REMINDED_KEY, Reminded};
            let mut reminded: Reminded =
                db.setting(REMINDED_KEY).await.ok().flatten().and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default();
            reminded.insert(chat.0, now);
            let json = serde_json::to_string(&reminded).unwrap_or_default();
            db.set_setting(REMINDED_KEY, &json).await
        });
        // It runs on the backend whether or not this waits for it.
        drop(save);
    }

    /// Takes down notifications for chats that have been read (here or on
    /// another device) and updates the Dock's unread count.
    fn sync_notifications(&mut self) {
        let Some(notifier) = self.notifier.clone() else { return };
        let all = || self.conversations.iter().chain(&self.archived);
        let read: Vec<ConversationId> = all()
            .filter(|c| c.unread_count == 0 && !c.unread_poll_votes && self.shown_notifications.contains_key(&c.id))
            .map(|c| c.id.clone())
            .collect();
        for id in read {
            if let Some(ids) = self.shown_notifications.remove(&id) {
                notifier.remove(&ids);
            }
        }
        // As Desktop: unread messages in unmuted chats, and one per chat marked unread.
        let badge: usize = all()
            .filter(|c| !c.muted)
            .map(|c| if c.unread_count == 0 && c.marked_unread { 1 } else { c.unread_count as usize })
            .sum();
        notifier.set_badge(badge);
    }

    fn start_catch_up(&mut self, cx: &mut Context<Self>) {
        let since = chrono::Utc::now().timestamp_millis();
        self.catching_up = Some(CatchUp { since, first: None, latest: None });
        // A connection that never opens doesn't keep the app closed.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CATCH_UP_CONNECT_TIMEOUT).await;
            let _ = this.update(cx, |this, cx| {
                let started = this.catching_up.as_ref().is_some_and(|c| c.since == since);
                if started && this.connection == ConnectionState::Connecting {
                    this.end_catch_up(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn end_catch_up(&mut self, cx: &mut Context<Self>) {
        if self.catching_up.take().is_some()
            && let Some((conversation, message_id, mentions)) = self.held_notification.take()
        {
            self.maybe_notify(conversation, message_id, mentions, cx);
        }
    }

    /// The app is waiting on the history transfer or the catch-up.
    pub fn is_loading(&self) -> bool {
        self.syncing_history().is_some() || self.catching_up.is_some()
    }

    /// The history transfer is running; the app waits on it (see `views::syncing`).
    pub fn syncing_history(&self) -> Option<&signal_backend::HistoryProgress> {
        use signal_backend::HistoryProgress;
        self.history
            .as_ref()
            .filter(|p| matches!(p, HistoryProgress::Waiting | HistoryProgress::Downloading { .. } | HistoryProgress::Importing { .. }))
    }

    fn on_history(&mut self, progress: signal_backend::HistoryProgress, cx: &mut Context<Self>) {
        use signal_backend::HistoryProgress;
        match &progress {
            HistoryProgress::Failed(error) => {
                self.history = None;
                self.report_error(format!("Couldn't transfer your messages from your phone: {error}"), cx);
            }
            HistoryProgress::Skipped => self.history = None,
            HistoryProgress::Done { .. } => {
                // The transfer brings the phone's chat colors and folders.
                self.reload_chat_colors(cx);
                self.reload_chat_folders(cx);
                self.history = Some(progress);
                // Shown for a moment, then out of the way.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(6)).await;
                    let _ = this.update(cx, |this, cx| {
                        this.history = None;
                        cx.notify();
                    });
                })
                .detach();
            }
            _ => self.history = Some(progress),
        }
        cx.notify();
    }

    pub fn set_notification_setting(&mut self, setting: crate::notifications::NotificationSetting, cx: &mut Context<Self>) {
        self.notification_setting = setting;
        let db = self.backend.db().clone();
        let write = self
            .backend
            .spawn(async move { db.set_setting(crate::notifications::NotificationSetting::KEY, setting.as_setting()).await });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                tracing::warn!(%error, "failed to save notification setting");
            }
        })
        .detach();
        cx.notify();
    }

    pub fn handle(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::NeedsLinking => {
                // Desktop ignores Signal links until it's linked.
                self.pending_links.clear();
                self.screen = Screen::Link { url: None, error: None };
                self.backend.send(Command::Link);
            }
            Event::LinkUrl(url) => self.screen = Screen::Link { url: Some(url), error: None },
            Event::LinkFailed(error) => {
                self.screen = Screen::Link {
                    url: None,
                    error: Some(error),
                }
            }
            Event::Ready { phone, aci } => {
                self.account = Some(Account { phone, aci });
                self.load_settings(cx);
                self.screen = Screen::Main;
                self.reload_conversations(cx);
                self.reload_stories(cx);
                self.reload_call_links(cx);
                self.reload_own_profile(cx);
                self.reload_badges(cx);
                self.start_catch_up(cx);
                self.start_unread_reminders(cx);
                for url in std::mem::take(&mut self.pending_links) {
                    self.open_signal_link(&url, cx);
                }
            }
            Event::Connection(state) => {
                // Desktop shows the app once the queue is drained, or when it
                // can't connect at all.
                if matches!(state, ConnectionState::Online | ConnectionState::Offline) {
                    self.end_catch_up(cx);
                }
                self.connection = state;
            }
            Event::CatchingUp { first, latest } => {
                if let Some(catch_up) = &mut self.catching_up {
                    catch_up.first = Some(first);
                    catch_up.latest = Some(latest);
                }
            }
            Event::ConversationsChanged => self.reload_conversations(cx),
            Event::StoriesChanged => self.reload_stories(cx),
            Event::StorySettingsChanged | Event::AccountSettingsChanged => self.load_settings(cx),
            Event::Megaphones(megaphones) => {
                self.megaphones = megaphones;
                cx.notify();
            }
            Event::MessageChanged {
                conversation,
                message_id,
            } => {
                // (The backend ends the sender's typing indicator itself.)
                cx.emit(AppEvent::MessageChanged {
                    conversation,
                    message_id,
                });
                self.reload_conversations(cx);
            }
            Event::MessageRemoved { conversation, message_id } => {
                cx.emit(AppEvent::MessageRemoved { conversation, message_id });
                self.reload_conversations(cx);
            }
            Event::PinsChanged(conversation) => cx.emit(AppEvent::PinsChanged(conversation)),
            Event::SafetyNumber { conversation, result } => {
                self.safety_number = Some((conversation, result));
                // Verification may have changed who a send asks about.
                self.reload_conversations(cx);
            }
            Event::KeyTransparency { conversation, verified } => {
                // Only the check this screen is waiting for.
                if self.kt_status == Some((conversation.clone(), KtStatus::Running)) {
                    self.kt_status = Some((conversation, if verified { KtStatus::Ok } else { KtStatus::Fail }));
                }
            }
            Event::KeyTransparencyFailed => cx.emit(AppEvent::KeyTransparencyFailed),
            Event::CaptchaRequired => cx.emit(AppEvent::CaptchaRequired),
            Event::Captcha(result) => {
                self.captcha_pending = false;
                cx.emit(AppEvent::CaptchaDone);
                if let Err(error) = result {
                    self.report_error(format!("Verification failed: {error}"), cx);
                }
                cx.notify();
            }
            Event::ServerAlerts(alerts) => {
                self.server_alerts = alerts;
                cx.notify();
            }
            Event::CriticalAlertModal => cx.emit(AppEvent::CriticalAlertModal),
            Event::ClockSkew(skewed) => {
                self.clock_skew = skewed;
                cx.notify();
            }
            Event::Call(update) => self.on_call(update, cx),
            Event::CallLinksChanged => self.reload_call_links(cx),
            Event::CallLinkCreated(room_id) => {
                self.reload_call_links(cx);
                cx.emit(AppEvent::CallLinkCreated(room_id));
            }
            Event::CallLinkOpened(room_id) => {
                self.reload_call_links(cx);
                self.open_call_lobby(ConversationId::call_link(&room_id), true, cx);
            }
            // Desktop's words for both.
            Event::CallLinkRefused { removed } => self.report_error(
                if removed { "Someone has removed you from the call." } else { "Your request to join this call has been denied." },
                cx,
            ),
            Event::CallDevices(devices) => self.call_devices = Some(devices),
            Event::CallReactions(reactions) => {
                let now = std::time::Instant::now();
                self.call_reactions.retain(|(at, _)| now.duration_since(*at) < std::time::Duration::from_secs(10));
                self.call_reactions.extend(reactions.into_iter().map(|r| (now, r)));
            }
            Event::CallMuted { by, target } => {
                // Someone muted us: the microphone shows off.
                if target.is_none() {
                    self.call_audio = false;
                }
                self.call_mute = Some((std::time::Instant::now(), by, target));
            }
            Event::CameraFailed(error) => {
                if let Some(lobby) = &mut self.lobby {
                    lobby.video = false;
                }
                self.call_video = false;
                self.report_error(format!("Couldn't start your camera: {error}"), cx);
            }
            Event::ShareSources(sources) => self.share_sources = Some(sources),
            Event::GroupPeek { conversation, users } => {
                if let Some(lobby) = self.lobby.as_mut().filter(|l| l.conversation == conversation) {
                    lobby.peek = Some(users);
                }
            }
            Event::History(progress) => self.on_history(progress, cx),
            Event::ViewOnceOpened(opened) => self.view_once(opened, cx),
            Event::GroupCreated(conversation) => {
                self.reload_conversations(cx);
                self.select(Some(conversation), cx);
            }
            Event::GroupLinkPreview(preview) => cx.emit(AppEvent::GroupLinkPreview(std::rc::Rc::new(preview))),
            Event::GroupOpened { conversation, toast } => {
                self.reload_conversations(cx);
                self.select(Some(conversation), cx);
                if let Some(toast) = toast {
                    cx.emit(AppEvent::Info(toast.into()));
                }
            }
            Event::GroupLinkFailed { title, description } => {
                cx.emit(AppEvent::Alert { title: title.into(), description: description.into() })
            }
            Event::Incoming { conversation, message_id, mentions_me, replies_to_me } => {
                self.maybe_notify(conversation, message_id, Mentions { mentions_me, replies_to_me }, cx)
            }
            Event::Reacted { conversation, message_id, author, sender, emoji } => {
                self.notify_reaction(conversation, message_id, author, sender, emoji, cx)
            }
            Event::PollActivity { conversation, message_id, sender, ended } => {
                self.notify_poll(conversation, message_id, sender, ended, cx)
            }
            Event::ConversationCleared(conversation) => {
                cx.emit(AppEvent::ConversationCleared(conversation));
                self.reload_conversations(cx);
            }
            Event::ConversationRemoved(conversation) => {
                self.typing.remove(&conversation);
                if self.selected.as_ref() == Some(&conversation) {
                    self.select(None, cx);
                }
                self.reload_conversations(cx);
            }
            Event::Typing {
                conversation,
                author,
                started,
            } => {
                let entry = self.typing.entry(conversation.clone()).or_default();
                entry.retain(|(a, _)| *a != author);
                if started {
                    entry.push((author, Instant::now() + TYPING_TIMEOUT));
                    // Re-render once it expires.
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(TYPING_TIMEOUT).await;
                        let _ = this.update(cx, |this, cx| {
                            this.expire_typing();
                            cx.notify();
                        });
                    })
                    .detach();
                }
                cx.emit(AppEvent::TypingChanged { conversation });
            }
            Event::Gifs { query, offset, result } => cx.emit(AppEvent::Gifs { query, offset, result }),
            Event::Stickers(packs) => {
                self.sticker_packs = packs.into_iter().map(std::sync::Arc::new).collect();
                // The open pack follows an install or removal.
                if let Some(StickerPreview::Ready(preview)) = &mut self.sticker_preview {
                    preview.pack.installed = self.sticker_packs.iter().any(|p| p.id == preview.pack.id);
                }
                cx.notify();
            }
            Event::StickerManager(view) => {
                self.sticker_manager = Some(view);
                cx.notify();
            }
            Event::StickerPackPreview { id, result } => {
                if matches!(&self.sticker_preview, Some(StickerPreview::Loading(loading)) if *loading == id) {
                    self.sticker_preview = Some(match result {
                        Ok(preview) => StickerPreview::Ready(Box::new(preview)),
                        Err(error) => StickerPreview::Failed(error),
                    });
                    cx.notify();
                }
            }
            Event::StickerUpload(upload) => cx.emit(AppEvent::StickerUpload(upload)),
            Event::RecipientFound { query, result } if self.link_lookup.as_ref() == Some(&query) => {
                self.link_lookup = None;
                match result {
                    Ok(entry) => self.start_conversation(entry, cx),
                    Err(description) => {
                        let title = if description == signal_backend::routes::UNKNOWN_LINK {
                            signal_backend::routes::UNKNOWN_LINK_TITLE
                        } else {
                            ""
                        };
                        cx.emit(AppEvent::Alert { title: title.into(), description: description.into() });
                    }
                }
            }
            Event::RecipientFound { query, result } => cx.emit(AppEvent::RecipientFound { query, result }),
            Event::LinkPreview { url, preview } => {
                cx.emit(AppEvent::LinkPreview { url, preview: preview.map(std::rc::Rc::new) })
            }
            Event::AuthorNamesChanged(names) => cx.emit(AppEvent::AuthorNamesChanged(names.into())),
            Event::ChatFoldersChanged => self.reload_chat_folders(cx),
            // Limits and features read it when drawn: just redraw.
            Event::RemoteConfigChanged => cx.emit(AppEvent::ConversationsChanged),
            Event::ProfileChanged(service_id) => {
                self.reload_badges(cx);
                cx.emit(AppEvent::ProfileChanged(service_id));
            }
            Event::OwnProfileChanged => {
                self.reload_own_profile(cx);
                self.reload_badges(cx);
            }
            Event::BadgesChanged => self.reload_badges(cx),
            Event::ProfileSaved(result) => cx.emit(AppEvent::ProfileSaved(result)),
            Event::UsernameReserved { nickname, discriminator, result } => {
                cx.emit(AppEvent::UsernameReserved { nickname, discriminator, result })
            }
            Event::UsernameSaved(result) => cx.emit(AppEvent::UsernameSaved(result)),
            Event::UsernameLinkReset(result) => cx.emit(AppEvent::UsernameLinkReset(result)),
            Event::ContactsSynced => {
                self.contact_sync = ContactSync::Idle;
                self.load_prefs(cx);
                self.reload_conversations(cx);
            }
            Event::Error(error) => {
                tracing::warn!("backend error: {error}");
                self.report_error(error, cx);
            }
            // Desktop's toasts for a spam report (`ToastType.ReportedSpam`
            // and `ReportedSpamAndBlocked`).
            Event::MessageRequestAnswered { answer, .. } => {
                let toast = match answer {
                    signal_store::MessageRequestResponse::Spam => Some("Reported as spam."),
                    signal_store::MessageRequestResponse::BlockAndSpam => Some("Reported as spam and blocked."),
                    _ => None,
                };
                if let Some(toast) = toast {
                    cx.emit(AppEvent::Info(toast.into()));
                }
                self.reload_conversations(cx);
            }
            Event::Unlinked => {
                // The old account's notifications and badge go too.
                if let Some(notifier) = self.notifier.clone() {
                    let mut shown: Vec<String> = self.shown_notifications.drain().flat_map(|(_, ids)| ids).collect();
                    shown.extend(self.reaction_notifications.drain().flat_map(|(_, ids)| ids));
                    shown.push(INCOMING_CALL_NOTIFICATION.to_owned());
                    notifier.remove(&shown);
                    notifier.set_badge(0);
                }
                self.account = None;
                // Its megaphones (the backend's check ended with it).
                self.megaphones.clear();
                // And its call (the backend's ended with it).
                self.call = None;
                self.lobby = None;
                self.call_links.clear();
                self.ringtone = None;
                self.conversations.clear();
                self.archived.clear();
                self.person_avatars.clear();
                self.own_profile = None;
                self.badges = Badges::default();
                self.contact_avatars.clear();
                self.contact_sync = ContactSync::Idle;
                self.selected = None;
                // The database was cleared; so are the next account's caches.
                self.typing.clear();
                self.emoji_usage = EmojiUsageCache::default();
                self.recent_stickers.clear();
                self.sticker_packs.clear();
                self.sticker_manager = None;
                self.sticker_preview = None;
                self.unverified.clear();
                self.captcha_pending = false;
                self.server_alerts = Default::default();
                self.clock_skew = false;
                self.chat_colors = Default::default();
                self.chat_folders.clear();
                self.selected_folder = None;
                self.link_lookup = None;
                self.pending_links.clear();
                self.has_giphy_key = std::env::var_os("GIPHY_API_KEY").is_some();
                self.image_optimization = Default::default();
                self.prefs = Default::default();
                self.pending_prefs.clear();
                crate::prefs::apply(self.prefs.theme, self.prefs.zoom, cx);
                self.seen_admin_delete_education = false;
                self.kt_status = None;
                self.key_transparency_disabled = false;
                self.kt_onboarding_seen = false;
                self.history = None;
                self.catching_up = None;
                self.held_notification = None;
                self.reminder_task = None;
                self.screen = Screen::Link { url: None, error: None };
                self.backend.send(Command::Link);
                cx.emit(AppEvent::SelectionChanged);
            }
        }
        cx.notify();
    }

    fn expire_typing(&mut self) {
        let now = Instant::now();
        for authors in self.typing.values_mut() {
            authors.retain(|(_, until)| *until > now);
        }
        self.typing.retain(|_, authors| !authors.is_empty());
    }

    pub fn typing_in(&self, conversation: &ConversationId) -> Vec<String> {
        let now = Instant::now();
        self.typing
            .get(conversation)
            .map(|authors| {
                authors
                    .iter()
                    .filter(|(_, until)| *until > now)
                    .map(|(author, _)| author.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Reloads the conversation list, coalescing bursts (e.g. initial sync).
    pub fn reload_conversations(&mut self, cx: &mut Context<Self>) {
        if self.reload_task.is_some() {
            self.reload_pending = true;
            return;
        }
        let db = self.backend.db().clone();
        let query = self
            .backend
            .spawn(async move {
                Ok::<_, signal_store::Error>((
                    db.conversations().await?,
                    db.archived_conversations().await?,
                    db.person_avatars().await?,
                    db.setting(signal_backend::SETTING_CONTACT_AVATARS).await?,
                    signal_backend::unverified_people(&db).await,
                ))
            });
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((conversations, archived, avatars, contact_avatars, unverified)) => {
                        this.conversations = conversations;
                        this.unverified = unverified;
                        this.archived = archived;
                        this.person_avatars = avatars.into_iter().map(|(sid, path)| (sid, path.into())).collect();
                        this.contact_avatars = contact_avatars
                            .and_then(|json| serde_json::from_str::<HashMap<String, std::path::PathBuf>>(&json).ok())
                            .unwrap_or_default();
                        this.sync_notifications();
                        cx.emit(AppEvent::ConversationsChanged);
                        if std::mem::take(&mut this.auto_select) && this.selected.is_none() {
                            let first = this.conversations.first().map(|c| c.id.clone());
                            this.select(first, cx);
                        }
                        cx.notify();
                    }
                    Err(e) => tracing::error!("loading conversations: {e}"),
                }
                this.reload_task = None;
                if std::mem::take(&mut this.reload_pending) {
                    this.reload_conversations(cx);
                }
            });
        }));
    }

    pub fn conversation(&self, id: &ConversationId) -> Option<&Conversation> {
        self.conversations.iter().chain(&self.archived).find(|c| &c.id == id)
    }

    fn load_settings(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let read = self.backend.spawn(async move {
            Ok::<_, signal_store::Error>((
                db.setting(signal_backend::SETTING_IMAGE_OPTIMIZATION).await?,
                db.setting(RECENT_STICKERS_SETTING).await?,
                db.setting(signal_backend::SETTING_GIPHY_API_KEY).await?,
                db.setting(crate::notifications::NotificationSetting::KEY).await?,
                db.setting(NAV_COLLAPSED_SETTING).await?,
                db.setting(HIDDEN_STORIES_SETTING).await?,
                db.setting(signal_backend::SETTING_STORIES_DISABLED).await?,
                db.setting(signal_backend::SETTING_STORY_VIEW_RECEIPTS).await?,
                (
                    db.setting(signal_backend::SETTING_READ_RECEIPTS).await?,
                    db.setting(signal_backend::SETTING_STORY_LISTS).await?,
                    db.setting(signal_backend::SETTING_MY_STORY_PRIVACY_SET).await?,
                    db.setting(ADMIN_DELETE_EDUCATION_SETTING).await?,
                    db.setting(signal_backend::SETTING_TYPING_INDICATORS).await?,
                    db.story_send_modes().await?,
                    db.setting(signal_backend::SETTING_KEY_TRANSPARENCY_DISABLED).await?,
                    db.setting(signal_backend::SETTING_KEY_TRANSPARENCY_ONBOARDING_SEEN).await?,
                ),
            ))
        });
        cx.spawn(async move |this, cx| {
            let (optimization, recents, giphy, notifications, collapsed, hidden, stories_off, view_receipts, more) =
                read.await.unwrap_or_default();
            let (read_receipts, lists, privacy_set, admin_delete_seen, typing, send_modes, kt_disabled, kt_seen) = more;
            let _ = this.update(cx, |this, cx| {
                this.read_receipts = signal_backend::setting_on(read_receipts.as_deref());
                this.typing_indicators = signal_backend::setting_on(typing.as_deref());
                this.story_lists = lists.and_then(|l| serde_json::from_str(&l).ok()).unwrap_or_default();
                this.my_story_privacy_set = privacy_set.as_deref() == Some("true");
                this.story_send_modes = send_modes.into_iter().collect();
                this.nav_collapsed = collapsed.as_deref() == Some("true");
                this.hidden_stories = hidden
                    .and_then(|h| serde_json::from_str::<Vec<String>>(&h).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(ConversationId)
                    .collect();
                this.stories_disabled = stories_off.as_deref() == Some("true");
                this.seen_admin_delete_education = admin_delete_seen.as_deref() == Some("true");
                this.key_transparency_disabled = kt_disabled.as_deref() == Some("true");
                this.kt_onboarding_seen = kt_seen.as_deref() == Some("true");
                this.story_view_receipts = view_receipts.map(|v| v != "false");
                this.notification_setting = crate::notifications::NotificationSetting::from_setting(notifications.as_deref());
                this.has_giphy_key |= giphy.is_some_and(|k| !k.trim().is_empty());
                this.image_optimization = signal_backend::media::Optimization::from_setting(optimization.as_deref());
                this.recent_stickers = recents.as_deref().map(parse_recent_stickers).unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
        self.load_prefs(cx);
        self.reload_chat_colors(cx);
        self.reload_chat_folders(cx);
    }

    /// Opens a sticker pack by its id and key (from the sticker manager, or
    /// a `signal.art` or `sgnl://addstickers` link through
    /// [`Self::open_signal_link`]): Desktop's
    /// `StickerPreviewModal` shows while its stickers load.
    pub fn open_sticker_pack(&mut self, id: Vec<u8>, key: Vec<u8>, cx: &mut Context<Self>) {
        self.sticker_preview = Some(StickerPreview::Loading(id.clone()));
        self.send(Command::PreviewStickerPack { id, key });
        cx.emit(AppEvent::OpenStickerPack);
        cx.notify();
    }

    /// Reads the chat folders again.
    pub fn reload_chat_folders(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let read = self.backend.spawn(async move { db.chat_folders().await });
        cx.spawn(async move |this, cx| {
            let Ok(folders) = read.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.chat_folders = folders;
                // A folder deleted elsewhere: back to all chats.
                if this.selected_folder().is_none() {
                    this.selected_folder = None;
                }
                cx.emit(AppEvent::ConversationsChanged);
                cx.notify();
            });
        })
        .detach();
    }

    /// The current folders, in order.
    pub fn current_folders(&self) -> Vec<signal_store::ChatFolder> {
        signal_store::current_folders(&self.chat_folders)
    }

    /// Desktop shows the folders above the chats once there's a custom one
    /// (`hasAnyCurrentCustomChatFolders`).
    pub fn has_custom_folders(&self) -> bool {
        self.chat_folders.iter().any(|f| f.is_current() && f.folder_type == signal_store::ChatFolderType::Custom)
    }

    /// The selected folder, while it's current.
    pub fn selected_folder(&self) -> Option<&signal_store::ChatFolder> {
        let id = self.selected_folder.as_ref()?;
        self.chat_folders.iter().find(|f| &f.id == id && f.is_current())
    }

    /// Shows a folder's chats (`None`: all).
    pub fn select_folder(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.selected_folder = id;
        cx.emit(AppEvent::ConversationsChanged);
        cx.notify();
    }

    /// Whether `chat` is in `folder` (Desktop's `isConversationInChatFolder`).
    pub fn in_folder(folder: &signal_store::ChatFolder, chat: &Conversation) -> bool {
        folder.contains(&Self::folder_chat(chat))
    }

    /// What a folder asks of `chat`. Muted is `Conversation::muted`, which
    /// an ended mute's past `muted_until` doesn't set.
    pub fn folder_chat(chat: &Conversation) -> signal_store::FolderChat<'_> {
        signal_store::FolderChat {
            id: &chat.id,
            kind: chat.kind,
            unread: chat.unread_count > 0 || chat.marked_unread,
            muted: chat.muted,
        }
    }

    /// Changes the folders: here at once, then saved and synced by the backend.
    pub fn edit_chat_folders(&mut self, edit: signal_store::ChatFolderEdit, cx: &mut Context<Self>) {
        if !signal_store::apply_folder_edit(&mut self.chat_folders, edit.clone()).is_empty() {
            cx.emit(AppEvent::ConversationsChanged);
            cx.notify();
        }
        self.send(Command::EditChatFolders(edit));
    }

    /// Reads the chat colors again.
    pub fn reload_chat_colors(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let read = self.backend.spawn(async move { db.chat_colors().await });
        cx.spawn(async move |this, cx| {
            let Ok(colors) = read.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.chat_colors = colors;
                cx.notify();
            });
        })
        .detach();
    }

    /// Changes the chat colors (Desktop's `ChatColorPicker` actions): here
    /// at once, then saved. They stay on this device, as Desktop's do.
    pub fn edit_chat_colors(&mut self, edit: signal_store::ChatColorEdit, cx: &mut Context<Self>) {
        if !self.chat_colors.apply(edit.clone()) {
            return;
        }
        cx.notify();
        let db = self.backend.db().clone();
        let write = self.backend.spawn(async move { db.edit_chat_colors(edit).await });
        cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                let _ = this.update(cx, |this, cx| {
                    this.report_error(format!("Couldn't save the chat color: {error}"), cx);
                    this.reload_chat_colors(cx);
                });
            }
        })
        .detach();
    }

    /// What our bubbles in `chat` are filled with.
    pub fn bubble_fill(&self, chat: &ConversationId) -> crate::chat_color::BubbleFill {
        crate::chat_color::fill(self.chat_colors.of(chat))
    }

    /// Reads Settings' other values, and applies the theme and zoom.
    fn load_prefs(&mut self, cx: &mut Context<Self>) {
        self.prefs_read += 1;
        let generation = self.prefs_read;
        let db = self.backend.db().clone();
        let read = self.backend.spawn(async move {
            let mut values = HashMap::new();
            for key in crate::prefs::Prefs::keys() {
                values.insert(key.to_owned(), db.setting(key).await?);
            }
            Ok::<_, signal_store::Error>(values)
        });
        cx.spawn(async move |this, cx| {
            let Ok(mut values) = read.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.prefs_read != generation {
                    return;
                }
                // A setting still being saved may have been read as it was;
                // once it reads back as set, it's saved.
                // One not read back within `PENDING_PREF_TIMEOUT` failed to
                // save (or was saved in another form): the database wins.
                this.pending_prefs.retain(|key, (value, since)| {
                    since.elapsed() < PENDING_PREF_TIMEOUT && values.get(*key).and_then(|v| v.as_deref()) != Some(value.as_str())
                });
                for (key, (value, _)) in &this.pending_prefs {
                    values.insert((*key).to_owned(), Some(value.clone()));
                }
                this.prefs = crate::prefs::Prefs::from_values(&values);
                crate::prefs::apply(this.prefs.theme, this.prefs.zoom, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Keeps `value` over what a reload reads for `key` until it reads back.
    fn mark_pending(&mut self, key: &'static str, value: String) {
        self.pending_prefs.insert(key, (value, std::time::Instant::now()));
    }

    /// Saves one of Settings' values; until it's written, a reload of the
    /// settings keeps the new value rather than reading the old one back.
    fn save_pref(&mut self, key: &'static str, value: String, cx: &mut Context<Self>) {
        self.mark_pending(key, value.clone());
        let db = self.backend.db().clone();
        let save = self.backend.spawn(async move { db.set_setting(key, &value).await });
        // It stays pending until a reload reads it back as saved: a reload
        // that read before the write landed may finish after it.
        cx.background_spawn(async move {
            if let Err(error) = save.await {
                tracing::warn!(%error, key, "failed to save a setting");
            }
        })
        .detach();
    }

    /// Desktop's Theme choice (Appearance).
    pub fn set_theme(&mut self, theme: crate::prefs::ThemeSetting, cx: &mut Context<Self>) {
        self.prefs.theme = theme;
        crate::prefs::apply(theme, self.prefs.zoom, cx);
        self.save_pref(signal_backend::SETTING_THEME, theme.as_setting().into(), cx);
        cx.notify();
    }

    /// Desktop's Zoom level (Appearance).
    pub fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.prefs.zoom = zoom;
        crate::prefs::apply(self.prefs.theme, zoom, cx);
        self.save_pref(crate::prefs::ZOOM_KEY, zoom.to_string(), cx);
        cx.notify();
    }

    /// Reads whether the app opens at login (Settings asks as it opens).
    pub fn refresh_open_at_login(&mut self, cx: &mut Context<Self>) {
        self.open_at_login = (self.login_item.is_enabled)();
        cx.notify();
    }

    /// Desktop's "Open at computer login".
    pub fn set_open_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        match (self.login_item.set)(on) {
            Ok(()) => self.open_at_login = on,
            Err(error) => self.report_error(format!("Couldn't change opening at login: {error}"), cx),
        }
        cx.notify();
    }

    /// The stories still up, from the store.
    pub fn reload_stories(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let now = chrono::Utc::now().timestamp_millis();
        let query = self.backend.spawn(async move { db.stories(now).await });
        cx.spawn(async move |this, cx| {
            if let Ok(stories) = query.await {
                let _ = this.update(cx, |this, cx| {
                    this.stories = stories;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn reload_call_links(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let query = self.backend.spawn(async move { db.call_links().await });
        cx.spawn(async move |this, cx| {
            if let Ok(links) = query.await {
                let _ = this.update(cx, |this, cx| {
                    this.call_links = links;
                    cx.emit(AppEvent::CallLinksChanged);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// A call link we keep, by room id.
    pub fn call_link(&self, room_id: &str) -> Option<&signal_store::CallLink> {
        self.call_links.iter().find(|l| l.room_id == room_id)
    }

    /// What a call is called: its chat's title, or a call link's name
    /// (Desktop's "Signal Call" when it has none).
    pub fn call_title(&self, id: &ConversationId) -> String {
        match id.as_call_link() {
            Some(room) => crate::views::calls::link_title(self.call_link(room)),
            None => self.conversation(id).map_or_else(|| "Unknown".to_owned(), |c| c.title.clone()),
        }
    }

    /// A group link (`https://signal.group/#…`, `sgnl://signal.group/#…` or
    /// `sgnl://joingroup/#…`), from a message, a paste or the OS: Desktop's
    /// join dialog shows the group, or its chat opens if we're in it.
    pub fn open_group_link(&self, url: String) {
        self.send(Command::OpenGroupLink { url });
    }

    /// Opens one of Signal's links (from a message, or from macOS for
    /// `sgnl://`), as Desktop's `handleSignalRoute` does (app/main.main.ts):
    /// a chat by number or username link, a group's join dialog, a call
    /// link's lobby. Returns `false` for a link that isn't Signal's, which
    /// opens in the browser. Before linking, Desktop ignores them.
    pub fn open_signal_link(&mut self, url: &str, cx: &mut Context<Self>) -> bool {
        use signal_backend::routes::{self, Route};
        let Some(route) = routes::parse(url) else { return false };
        if self.account.is_none() {
            tracing::info!("ignoring a Signal link before linking");
            return true;
        }
        match route {
            Route::ContactByPhoneNumber(_) | Route::ContactByEncryptedUsername(_) => {
                self.link_lookup = Some(url.to_owned());
                self.send(Command::FindRecipient { query: url.to_owned() });
            }
            Route::GroupInvite(code) => self.open_group_link(Route::group_link(&code)),
            Route::LinkCall(key) => self.open_call_link(Route::call_link(&key)),
            // Desktop shows the pack's preview (`showStickerPackPreview`).
            Route::AddStickers { pack_id, pack_key } => match signal_backend::stickers::pack_of(&pack_id, &pack_key) {
                Some((id, key)) => self.open_sticker_pack(id, key, cx),
                None => cx.emit(AppEvent::Alert {
                    title: "Sticker Pack".into(),
                    description: "Error opening sticker pack. Check your internet connection and try again.".into(),
                }),
            },
            // The window is shown by whoever handed us the link.
            Route::ShowWindow => {}
            // A solved captcha goes to the server (`challenge.rs`).
            Route::Captcha(captcha) => {
                self.captcha_pending = true;
                self.send(Command::SubmitCaptcha(captcha));
                cx.notify();
            }
            Route::Unsupported(name) => {
                tracing::info!(route = name, "unsupported Signal link");
                cx.emit(AppEvent::Alert { title: routes::UNKNOWN_LINK_TITLE.into(), description: routes::UNKNOWN_LINK.into() });
            }
        }
        true
    }

    /// A link macOS opened us with (`sgnl://…`, or one of Signal's https
    /// links handed over): opened now, or once the account has loaded. A
    /// link that isn't one of Signal's routes is ignored, as Desktop's
    /// `open-url` handler ignores it.
    pub fn open_link_from_system(&mut self, url: String, cx: &mut Context<Self>) {
        if signal_backend::routes::parse(&url).is_none() {
            tracing::info!("ignoring a link that isn't Signal's");
            return;
        }
        match self.screen {
            Screen::Loading => self.pending_links.push(url),
            _ => {
                self.open_signal_link(&url, cx);
            }
        }
    }

    /// A `https://signal.link/call/#key=…` URL (from a message, say): the
    /// backend keeps the link and its lobby opens.
    pub fn open_call_link(&mut self, url: String) {
        self.send(Command::OpenCallLink { url });
    }

    pub fn set_tab(&mut self, tab: NavTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            // Desktop's `markCallsTabViewed`: viewing the Calls tab reads
            // every call (and tells our other devices).
            if tab == NavTab::Calls {
                self.send(Command::MarkCallsRead { conversation: None });
            }
            cx.notify();
        }
    }

    /// Desktop's "Hide Tabs" / "Show Tabs".
    pub fn toggle_nav(&mut self, cx: &mut Context<Self>) {
        self.nav_collapsed = !self.nav_collapsed;
        self.save_setting(NAV_COLLAPSED_SETTING, if self.nav_collapsed { "true" } else { "false" }.into(), cx);
        cx.notify();
    }

    /// Hides (or shows again) a person's or group's stories.
    /// Hides (or shows again) a person's or group's stories, on every
    /// device (Storage Service's `hideStory`).
    pub fn set_story_hidden(&mut self, chat: ConversationId, hidden: bool, cx: &mut Context<Self>) {
        let changed = if hidden { self.hidden_stories.insert(chat.clone()) } else { self.hidden_stories.remove(&chat) };
        if changed {
            self.send(Command::SetChat { conversation: chat, change: signal_backend::ChatChange::HideStory(hidden) });
            cx.notify();
        }
    }

    /// Desktop's "Turn off stories" / "Turn on stories": off, none are kept
    /// or shown (ours are deleted for everyone), and the tab goes; for the
    /// whole account.
    pub fn set_stories_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.stories_disabled = disabled;
        if disabled && self.tab == NavTab::Stories {
            self.tab = NavTab::Chats;
        }
        if disabled {
            self.stories.clear();
        }
        self.send(Command::SetStoriesDisabled(disabled));
        cx.notify();
    }

    /// "Verify automatically": checks their key (Desktop's
    /// `checkKeyTransparency`), shown as running until the answer comes.
    pub fn check_key_transparency(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        self.kt_status = Some((conversation.clone(), KtStatus::Running));
        self.send(Command::CheckKeyTransparency { conversation });
        cx.notify();
    }

    /// The safety number screen closed: its check stops.
    pub fn close_safety_number(&mut self) {
        if matches!(self.kt_status, Some((_, KtStatus::Running))) {
            self.send(Command::CancelKeyTransparency);
        }
        self.kt_status = None;
    }

    /// Automatic key verification on or off, for the whole account.
    pub fn set_key_transparency_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.key_transparency_disabled = disabled;
        self.send(Command::SetKeyTransparencyDisabled(disabled));
        cx.notify();
    }

    /// Desktop's key transparency introduction was seen; it isn't shown again.
    pub fn set_kt_onboarding_seen(&mut self, cx: &mut Context<Self>) {
        if !self.kt_onboarding_seen {
            self.kt_onboarding_seen = true;
            self.save_setting(signal_backend::SETTING_KEY_TRANSPARENCY_ONBOARDING_SEEN, "true".into(), cx);
        }
    }

    /// Desktop's admin delete confirmation was confirmed; it isn't shown again.
    pub fn set_seen_admin_delete_education(&mut self, cx: &mut Context<Self>) {
        if !self.seen_admin_delete_education {
            self.seen_admin_delete_education = true;
            self.save_setting(ADMIN_DELETE_EDUCATION_SETTING, "true".into(), cx);
        }
    }

    /// Whether we tell people we viewed their stories (and see who viewed
    /// ours), for the whole account.
    pub fn set_story_view_receipts(&mut self, on: bool, cx: &mut Context<Self>) {
        self.story_view_receipts = Some(on);
        self.send(Command::SetStoryViewReceipts(on));
        cx.notify();
    }

    /// Desktop's Privacy toggles (read receipts, typing indicators), for
    /// the whole account.
    pub fn set_account_setting(&mut self, setting: signal_backend::AccountSetting, on: bool, cx: &mut Context<Self>) {
        use signal_backend::AccountSetting;
        self.prefs.flags.insert(setting, on);
        // The backend saves it; until then a reload keeps this value.
        self.mark_pending(setting.key(), on.to_string());
        match setting {
            AccountSetting::ReadReceipts => self.read_receipts = on,
            AccountSetting::TypingIndicators => {
                self.typing_indicators = on;
                // Nobody's shown typing any more.
                if !on {
                    for conversation in std::mem::take(&mut self.typing).into_keys() {
                        cx.emit(AppEvent::TypingChanged { conversation });
                    }
                }
            }
            _ => {}
        }
        self.send(Command::SetAccountSetting { setting, on });
        cx.notify();
    }

    /// Story view receipts are on: as set, else as read receipts are
    /// (Desktop's `areStoryViewReceiptsEnabled`).
    pub fn story_view_receipts_on(&self) -> bool {
        self.story_view_receipts.unwrap_or(self.read_receipts)
    }

    /// My Story's privacy, or as Desktop starts it.
    pub fn my_story(&self) -> signal_backend::StoryList {
        self.story_lists.iter().find(|l| l.is_my_story()).cloned().unwrap_or_else(signal_backend::StoryList::my_story)
    }

    /// The custom stories, in Storage Service's order.
    pub fn custom_story_lists(&self) -> Vec<signal_backend::StoryList> {
        self.story_lists.iter().filter(|l| !l.is_my_story()).cloned().collect()
    }

    /// Saves a story list (a new one is added), here at once and for the
    /// whole account.
    pub fn save_story_list(&mut self, list: signal_backend::StoryList, cx: &mut Context<Self>) {
        match self.story_lists.iter_mut().find(|l| l.id == list.id) {
            Some(existing) => *existing = list.clone(),
            None => self.story_lists.push(list.clone()),
        }
        if list.is_my_story() {
            self.my_story_privacy_set = true;
        }
        self.send(Command::SaveStoryList(list));
        cx.notify();
    }

    /// A group's story send mode (Desktop's `getStorySendMode`).
    pub fn story_send_mode(&self, group: &ConversationId) -> signal_store::StorySendMode {
        self.story_send_modes.get(group).copied().unwrap_or_default()
    }

    /// Desktop's `isGroupInStoryMode`: a group we're in whose story is on
    /// the list, always or (by default) while it has stories up.
    pub fn group_in_story_mode(&self, group: &Conversation) -> bool {
        use signal_store::StorySendMode;
        if group.kind != signal_store::ConversationKind::Group || group.left || group.ended {
            return false;
        }
        match self.story_send_mode(&group.id) {
            StorySendMode::Always => true,
            StorySendMode::Never => false,
            StorySendMode::IfActive => self.stories.iter().any(|s| s.group.as_ref() == Some(&group.id)),
        }
    }

    /// The group stories we can post to (Desktop's `getGroupStories`), by
    /// title.
    pub fn group_stories(&self) -> Vec<Conversation> {
        let mut groups: Vec<Conversation> =
            self.conversations.iter().chain(&self.archived).filter(|c| self.group_in_story_mode(c)).cloned().collect();
        groups.sort_by_key(|g| g.title.to_lowercase());
        groups
    }

    /// Groups we're in that aren't group stories yet (Desktop's
    /// `getNonGroupStories`), for "New group story".
    pub fn groups_without_stories(&self) -> Vec<Conversation> {
        let mut groups: Vec<Conversation> = self
            .conversations
            .iter()
            .chain(&self.archived)
            .filter(|c| c.kind == signal_store::ConversationKind::Group && !c.left && !c.ended && !c.blocked && !self.group_in_story_mode(c))
            .cloned()
            .collect();
        groups.sort_by_key(|g| g.title.to_lowercase());
        groups
    }

    /// Adds groups' stories to the list (`Always`) or removes them
    /// (`Never`), here at once and for the whole account.
    pub fn set_story_send_mode(&mut self, groups: Vec<ConversationId>, mode: signal_store::StorySendMode, cx: &mut Context<Self>) {
        for group in &groups {
            self.story_send_modes.insert(group.clone(), mode);
        }
        self.send(Command::SetStorySendMode { groups, mode });
        cx.notify();
    }

    /// Deletes one of our stories for everyone (Desktop's "Delete story").
    pub fn delete_story(&mut self, id: i64, cx: &mut Context<Self>) {
        self.stories.retain(|s| s.id != id);
        self.send(Command::DeleteStory { id });
        cx.notify();
    }

    /// Deletes a reply to a group story, here or (ours) for everyone.
    pub fn delete_story_reply(&mut self, story: i64, author: Option<String>, sent_at: i64, for_everyone: bool, cx: &mut Context<Self>) {
        self.send(Command::DeleteStoryReply { story, author, sent_at, for_everyone });
        cx.notify();
    }

    /// Deletes a custom story, and what was sent to it.
    pub fn delete_story_list(&mut self, id: String, cx: &mut Context<Self>) {
        self.story_lists.retain(|l| l.id != id || l.is_my_story());
        self.send(Command::DeleteStoryList { id });
        cx.notify();
    }

    fn save_setting(&self, key: &'static str, value: String, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let save = self.backend.spawn(async move { db.set_setting(key, &value).await });
        cx.background_spawn(async move {
            let _ = save.await;
        })
        .detach();
    }

    /// The chat a story belongs to in the list: its group, else its author
    /// (`None` for ours).
    pub fn story_chat(story: &signal_store::Story) -> Option<ConversationId> {
        story.group.clone().or_else(|| story.author.as_deref().map(ConversationId::direct))
    }

    /// The stories by who posted them (Desktop's `getStories`): one entry
    /// per person or group, its stories oldest first; unviewed first, then
    /// newest. Ours aren't included.
    pub fn story_groups(&self) -> Vec<StoryGroup> {
        let mut groups: Vec<StoryGroup> = Vec::new();
        for story in self.stories.iter().filter(|s| s.author.is_some()) {
            let Some(chat) = Self::story_chat(story) else { continue };
            match groups.iter_mut().find(|g| g.chat == chat) {
                Some(group) => group.stories.push(story.clone()),
                None => {
                    let title = match &story.group {
                        Some(group) => self.conversation(group).map(|c| c.title.clone()).unwrap_or_default(),
                        None => self
                            .conversation(&chat)
                            .map(|c| c.title.clone())
                            .or_else(|| story.author_name.clone())
                            .unwrap_or_default(),
                    };
                    groups.push(StoryGroup { hidden: self.hidden_stories.contains(&chat), chat, title, stories: vec![story.clone()] });
                }
            }
        }
        for group in &mut groups {
            group.stories.sort_by_key(|s| s.sent_at);
        }
        groups.sort_by_key(|g| (!g.unread(), std::cmp::Reverse(g.latest())));
        groups
    }

    /// Our own stories still up, oldest first.
    pub fn my_stories(&self) -> Vec<signal_store::Story> {
        let mut mine: Vec<_> = self.stories.iter().filter(|s| s.author.is_none()).cloned().collect();
        mine.sort_by_key(|s| s.sent_at);
        mine
    }

    /// Opens the story viewer at a person's or group's stories (`None`:
    /// ours).
    pub fn view_stories(&mut self, chat: Option<ConversationId>, cx: &mut Context<Self>) {
        self.pause_audio();
        cx.emit(AppEvent::ViewStories(chat));
    }

    /// Desktop's "Text story".
    pub fn create_text_story(&mut self, cx: &mut Context<Self>) {
        cx.emit(AppEvent::CreateStory(None));
    }

    /// Desktop's "Photo or video": a picked file, checked as Desktop checks
    /// it, opens the creator.
    pub fn create_media_story(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let content_type = signal_backend::media::content_type_for_file(&name).to_owned();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return self.report_error(format!("Couldn't read {name}: {error}"), cx),
        };
        if !content_type.starts_with("image/") && !content_type.starts_with("video/") {
            return self.report_error("A story can be a photo or a video.", cx);
        }
        if let Some(problem) = crate::views::story_creator::story_file_problem(&bytes, &content_type) {
            return self.report_error(problem, cx);
        }
        cx.emit(AppEvent::CreateStory(Some((std::sync::Arc::new(bytes), content_type, path))));
    }

    /// Desktop's Stories tab badge (`getStoriesNotificationCount`): chats
    /// with an unviewed story, not hidden.
    pub fn unread_story_chats(&self) -> usize {
        let mut chats = std::collections::HashSet::new();
        for story in self.stories.iter().filter(|s| !s.read && s.author.is_some()) {
            if let Some(chat) = Self::story_chat(story).filter(|c| !self.hidden_stories.contains(c)) {
                chats.insert(chat);
            }
        }
        chats.len()
    }

    /// Desktop's Chats tab badge: unread messages, and chats marked unread.
    pub fn unread_chats_badge(&self) -> u32 {
        self.conversations
            .iter()
            .filter(|c| !c.muted)
            .map(|c| if c.unread_count > 0 { c.unread_count } else { u32::from(c.marked_unread) })
            .sum()
    }

    /// Saves your Giphy API key.
    pub fn set_giphy_key(&mut self, key: String, cx: &mut Context<Self>) {
        let key = key.trim().to_owned();
        self.has_giphy_key = !key.is_empty();
        let db = self.backend.db().clone();
        let save = self.backend.spawn(async move { db.set_setting(signal_backend::SETTING_GIPHY_API_KEY, &key).await });
        cx.background_spawn(async move {
            let _ = save.await;
        })
        .detach();
        cx.notify();
    }

    /// A looping video's frames, `max_width` wide at most; `None` while it's
    /// being decoded (the state notifies when it's ready).
    pub fn video_loop(
        &mut self,
        path: &std::path::Path,
        max_width: u32,
        cx: &mut Context<Self>,
    ) -> Option<std::sync::Arc<gpui_kit::RenderImage>> {
        let key = (path.to_path_buf(), max_width);
        let now = Instant::now();
        match self.loops.get(&key, now) {
            Some(LoopState::Ready(image)) => return Some(image.clone()),
            Some(LoopState::Loading | LoopState::Failed) => return None,
            None => {}
        }
        self.loops.start(key.clone(), now);
        for image in self.loops.trim(now) {
            cx.drop_image(image, None);
        }
        let path = key.0.clone();
        let decode = cx.background_spawn(async move { crate::video::decode_loop(&path, max_width) });
        cx.spawn(async move |this, cx| {
            let result = decode.await;
            let _ = this.update(cx, |this, cx| {
                let (state, bytes) = match result {
                    Ok(image) => {
                        let size = image.size(0);
                        let bytes = image.frame_count() * size.width.0 as usize * size.height.0 as usize * 4;
                        (LoopState::Ready(std::sync::Arc::new(image)), bytes)
                    }
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "couldn't decode video");
                        (LoopState::Failed, 0)
                    }
                };
                this.loops.finish(&key, state, bytes);
                for image in this.loops.trim(Instant::now()) {
                    cx.drop_image(image, None);
                }
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// Sends a sticker (replying to `quote` when given) and remembers it
    /// as recently used.
    pub fn send_sticker(
        &mut self,
        conversation: ConversationId,
        pack: std::sync::Arc<signal_backend::StickerPackView>,
        sticker: signal_backend::StickerView,
        quote: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let key = (pack.id.clone(), sticker.id);
        self.recent_stickers.retain(|k| *k != key);
        self.recent_stickers.insert(0, key);
        self.recent_stickers.truncate(MAX_RECENT_STICKERS);
        let value = format_recent_stickers(&self.recent_stickers);
        let db = self.backend.db().clone();
        let save = self.backend.spawn(async move { db.set_setting(RECENT_STICKERS_SETTING, &value).await });
        cx.background_spawn(async move {
            let _ = save.await;
        })
        .detach();
        self.send(Command::SendSticker { conversation, pack, sticker, quote });
        cx.notify();
    }

    pub fn set_image_optimization(&mut self, level: signal_backend::media::Optimization, cx: &mut Context<Self>) {
        self.image_optimization = level;
        let db = self.backend.db().clone();
        let write = self
            .backend
            .spawn(async move { db.set_setting(signal_backend::SETTING_IMAGE_OPTIMIZATION, level.as_setting()).await });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                tracing::warn!(%error, "failed to save image optimization setting");
            }
        })
        .detach();
        cx.notify();
    }

    /// Opens a chat with someone (or a group) from the directory or a
    /// lookup, creating the conversation if needed.
    pub fn start_conversation(&mut self, entry: signal_store::DirectoryEntry, cx: &mut Context<Self>) {
        let id = entry.id.clone();
        if self.conversation(&id).is_some() {
            return self.select(Some(id), cx);
        }
        let db = self.backend.db().clone();
        let create = self.backend.spawn(async move {
            let update = signal_store::ConversationUpdate {
                kind: Some(entry.kind),
                title: Some(entry.title),
                phone: entry.phone,
                ..Default::default()
            };
            db.upsert_conversation(&entry.id, update).await
        });
        cx.spawn(async move |this, cx| {
            let created = create.await;
            let _ = this.update(cx, |this, cx| match created {
                Ok(()) => {
                    this.reload_conversations(cx);
                    this.select(Some(id), cx);
                }
                Err(error) => this.report_error(format!("Couldn't start the chat: {error}"), cx),
            });
        })
        .detach();
    }

    /// Plays or pauses an audio attachment (`path`, downloaded); another
    /// one playing stops, as in Desktop.
    pub fn toggle_audio(&mut self, message_id: i64, index: usize, path: std::path::PathBuf, title: String, cx: &mut Context<Self>) {
        if let Some(audio) = self.audio.as_ref().filter(|a| a.message_id == message_id && a.index == index) {
            if audio.player.is_playing() {
                audio.player.pause();
            } else {
                audio.player.play();
            }
            cx.notify();
            return;
        }
        self.start_audio(message_id, index, &path, 0., title, cx);
    }

    /// Plays from `fraction` of the way through (a click on the waveform).
    pub fn seek_audio(
        &mut self,
        message_id: i64,
        index: usize,
        path: std::path::PathBuf,
        fraction: f64,
        title: String,
        cx: &mut Context<Self>,
    ) {
        match self.audio.as_ref().filter(|a| a.message_id == message_id && a.index == index) {
            Some(audio) => {
                audio.player.seek(fraction * audio.player.duration());
                cx.notify();
            }
            None => self.start_audio(message_id, index, &path, fraction, title, cx),
        }
    }

    /// The mini player's play and pause.
    pub fn toggle_current_audio(&mut self, cx: &mut Context<Self>) {
        if let Some(audio) = &self.audio {
            if audio.player.is_playing() {
                audio.player.pause();
            } else {
                audio.player.play();
            }
            cx.notify();
        }
    }

    /// The mini player's close: stops it (Desktop's `unloadMessageAudio`).
    pub fn close_audio(&mut self, cx: &mut Context<Self>) {
        if self.audio.take().is_some() {
            cx.notify();
        }
    }

    /// Desktop's rate button: 1×, 1.5×, 2×, 0.5×.
    pub fn cycle_audio_rate(&mut self, cx: &mut Context<Self>) {
        if let Some(audio) = &mut self.audio {
            audio.rate = crate::audio::next_rate(audio.rate);
            audio.player.set_rate(audio.rate);
            cx.notify();
        }
    }

    fn start_audio(
        &mut self,
        message_id: i64,
        index: usize,
        path: &std::path::Path,
        fraction: f64,
        title: String,
        cx: &mut Context<Self>,
    ) {
        self.start_audio_as(message_id, index, path, fraction, title, true, 1., cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn start_audio_as(
        &mut self,
        message_id: i64,
        index: usize,
        path: &std::path::Path,
        fraction: f64,
        title: String,
        consecutive: bool,
        rate: f32,
        cx: &mut Context<Self>,
    ) {
        // The one playing stops first.
        self.audio = None;
        let player = match (self.open_audio)(path) {
            Ok(player) => player,
            Err(error) => return self.report_error(format!("Couldn't play it: {error:#}"), cx),
        };
        if fraction > 0. {
            player.seek(fraction * player.duration());
        }
        if rate != 1. {
            player.set_rate(rate);
        }
        player.play();
        // A voice message played is viewed (Desktop's `markViewed` from
        // its voice notes player); the backend sends the receipt once.
        self.send(Command::MarkViewed { message_id });
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUDIO_POLL).await;
                let ended = this.update(cx, |this, cx| {
                    let Some(audio) = &mut this.audio else { return true };
                    if audio.player.is_playing() {
                        audio.last_position = audio.player.position();
                        cx.notify();
                        return false;
                    }
                    // At the end it stops by itself (and rewinds): done.
                    // Paused (anywhere, the start too), it waits.
                    let ended = audio.player.position() <= 0.
                        && audio.last_position >= audio.player.duration() - END_SLACK;
                    if ended {
                        let (message_id, rate, consecutive) = (audio.message_id, audio.rate, audio.consecutive);
                        this.audio = None;
                        if consecutive {
                            this.play_next_audio(message_id, rate, cx);
                        }
                        cx.notify();
                    }
                    ended
                });
                if ended.unwrap_or(true) {
                    break;
                }
            }
        });
        self.audio = Some(AudioPlaying { message_id, index, title, player, rate, consecutive, last_position: 0., _poll: poll });
        cx.notify();
    }

    /// Desktop's consecutive playback (`messageAudioEnded`): the chat's next
    /// audio after the one that ended plays at the same rate, if it's
    /// downloaded; with none, Desktop's "confirm up" sound plays instead.
    fn play_next_audio(&mut self, after: i64, rate: f32, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let query = self.backend.spawn(async move { db.next_audio(after).await });
        cx.spawn(async move |this, cx| {
            let next = match query.await {
                Ok(next) => next,
                Err(error) => {
                    tracing::warn!(%error, "couldn't find the next audio");
                    None
                }
            };
            let _ = this.update(cx, |this, cx| {
                // Something else started meanwhile: it stays.
                if this.audio.is_some() {
                    return;
                }
                match next.as_ref().and_then(next_playable) {
                    Some((message_id, index, path, title)) => this.start_audio_as(message_id, index, &path, 0., title, true, rate, cx),
                    None => crate::sounds::play(crate::sounds::Sound::ConfirmUp),
                }
            });
        })
        .detach();
    }

    /// A downloaded audio file's waveform and length, decoding it (once, in
    /// the background) the first time it's asked for.
    pub fn waveform(&mut self, path: &std::path::Path, cx: &mut Context<Self>) -> Option<Waveform> {
        if let Some(known) = self.waveforms.get(path) {
            return known.clone();
        }
        self.waveforms.insert(path.to_owned(), None);
        let path = path.to_owned();
        let decode = cx.background_executor().spawn({
            let path = path.clone();
            async move { crate::audio::waveform(&path) }
        });
        cx.spawn(async move |this, cx| {
            let result = decode.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((peaks, seconds)) => {
                        this.waveforms.insert(path, Some((std::rc::Rc::new(peaks), seconds)));
                    }
                    Err(error) => tracing::warn!(error = %format!("{error:#}"), "couldn't read an audio file's waveform"),
                }
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// A downloaded video's first frame, made (once, in the background) the
    /// first time it's asked for.
    pub fn poster(&mut self, path: &std::path::Path, cx: &mut Context<Self>) -> Option<std::path::PathBuf> {
        if let Some(known) = self.posters.get(path) {
            return known.clone();
        }
        self.posters.insert(path.to_owned(), None);
        let path = path.to_owned();
        let make = cx.background_executor().spawn({
            let path = path.clone();
            async move { crate::video::poster(&path) }
        });
        cx.spawn(async move |this, cx| {
            let result = make.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(poster) => {
                        this.posters.insert(path, Some(poster));
                    }
                    Err(error) => tracing::warn!(error = %format!("{error:#}"), "couldn't make a video's poster"),
                }
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// Plays a video in the viewer; any audio playing pauses, as in Desktop.
    pub fn view_video(&mut self, video: crate::views::video_viewer::ViewedVideo, cx: &mut Context<Self>) {
        self.pause_audio();
        cx.emit(AppEvent::ViewVideo(video));
    }

    /// Pauses the audio playing, if any (for another sound, or recording).
    pub fn pause_audio(&self) {
        if let Some(audio) = &self.audio {
            audio.player.pause();
        }
    }

    /// A view-once message we opened: its copy in the viewer, deleted when
    /// the viewer closes.
    fn view_once(&mut self, opened: signal_backend::OpenedViewOnce, cx: &mut Context<Self>) {
        if opened.content_type.starts_with("video/") {
            let video = crate::views::video_viewer::ViewedVideo { path: opened.path, file_name: None, view_once: true };
            self.view_video(video, cx);
        } else {
            let size = opened.size.filter(|(w, h)| *w > 0 && *h > 0);
            let image = crate::views::image_viewer::ViewedImage { path: opened.path, size, file_name: None, view_once: true };
            self.view_image(image, cx);
        }
    }

    pub fn view_image(&self, image: crate::views::image_viewer::ViewedImage, cx: &mut Context<Self>) {
        cx.emit(AppEvent::ViewImage(image));
    }

    pub fn report_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        let error = error.into();
        cx.emit(AppEvent::Error(error.clone().into()));
        self.last_error = Some(error);
    }

    /// Pins or unpins for the whole account.
    pub fn set_pinned(&self, conversation: ConversationId, pinned: bool) {
        self.send(Command::SetPinned { conversation, pinned });
    }

    /// Opens `conversation` scrolled to `message_id`.
    /// Desktop's call buttons: the lobby first, camera on for a video call.
    pub fn open_call_lobby(&mut self, conversation: ConversationId, video: bool, cx: &mut Context<Self>) {
        if self.call.is_some() || self.lobby.is_some() {
            return;
        }
        if conversation.as_group().is_some() || conversation.as_call_link().is_some() {
            self.send(Command::PeekGroupCall { conversation: conversation.clone() });
        }
        self.lobby = Some(CallLobby { conversation, video, audio: true, peek: None });
        if video {
            self.send(Command::SetCallVideo(true));
        }
        cx.notify();
    }

    pub fn set_lobby_video(&mut self, video: bool, cx: &mut Context<Self>) {
        if let Some(lobby) = &mut self.lobby {
            lobby.video = video;
            self.backend.send(Command::SetCallVideo(video));
            cx.notify();
        }
    }

    pub fn set_lobby_audio(&mut self, audio: bool, cx: &mut Context<Self>) {
        if let Some(lobby) = &mut self.lobby {
            lobby.audio = audio;
            cx.notify();
        }
    }

    pub fn close_call_lobby(&mut self, cx: &mut Context<Self>) {
        if let Some(lobby) = self.lobby.take() {
            if lobby.video {
                self.send(Command::SetCallVideo(false));
            }
            cx.notify();
        }
    }

    /// The lobby's Start.
    pub fn start_call(&mut self, cx: &mut Context<Self>) {
        let Some(lobby) = self.lobby.take() else { return };
        self.call_audio = lobby.audio;
        self.call_video = lobby.video;
        self.call_minimized = false;
        self.send(Command::StartCall { conversation: lobby.conversation, video: lobby.video });
        if !lobby.audio {
            self.send(Command::SetCallAudio(false));
        }
        cx.notify();
    }

    /// Answers, with the camera on (`video`) or not.
    pub fn accept_call(&mut self, video: bool, cx: &mut Context<Self>) {
        self.call_audio = true;
        self.call_video = video;
        self.call_minimized = false;
        self.send(Command::AcceptCall { video });
        cx.notify();
    }

    pub fn decline_call(&mut self) {
        self.send(Command::DeclineCall);
    }

    pub fn hang_up(&mut self) {
        self.send(Command::HangUp);
    }

    pub fn toggle_call_audio(&mut self, cx: &mut Context<Self>) {
        self.call_audio = !self.call_audio;
        self.send(Command::SetCallAudio(self.call_audio));
        cx.notify();
    }

    pub fn toggle_call_video(&mut self, cx: &mut Context<Self>) {
        self.call_video = !self.call_video;
        self.send(Command::SetCallVideo(self.call_video));
        cx.notify();
    }

    pub fn set_call_minimized(&mut self, minimized: bool, cx: &mut Context<Self>) {
        self.call_minimized = minimized;
        cx.notify();
    }

    fn on_call(&mut self, update: Option<signal_backend::CallUpdate>, cx: &mut Context<Self>) {
        let ringing = update.as_ref().is_some_and(|c| c.incoming && c.status == signal_backend::CallStatus::Ringing);
        let was_ringing = self.call.as_ref().is_some_and(|c| c.incoming && c.status == signal_backend::CallStatus::Ringing);
        if ringing && !was_ringing {
            self.notify_incoming_call(update.as_ref(), cx);
            // Desktop's `playRingtone`: with "Play calling sounds" on,
            // whatever notifications show.
            if self.prefs.on(crate::prefs::Toggle::CallSounds) {
                self.ringtone = crate::ringtone::Ringtone::start();
            }
        }
        if !ringing {
            self.ringtone = None;
        }
        if !ringing && was_ringing && let Some(notifier) = &self.notifier {
            notifier.remove(&[INCOMING_CALL_NOTIFICATION.to_owned()]);
        }
        // A call (theirs, say) replaces the lobby, and its camera preview.
        // (Our own call takes the lobby first, in `start_call`.)
        if update.is_some()
            && let Some(lobby) = self.lobby.take()
            && lobby.video
        {
            self.send(Command::SetCallVideo(false));
        }
        if update.is_none() {
            self.call_audio = true;
            self.call_video = false;
            self.call_minimized = false;
            self.call_reactions.clear();
            self.call_mute = None;
        }
        self.call = update;
    }

    /// Desktop's incoming call notification ("Incoming voice call"),
    /// shown when Signal isn't in front (`notifyForCall`), silent (the
    /// ringtone plays), and gone once the call is answered or over.
    fn notify_incoming_call(&self, call: Option<&signal_backend::CallUpdate>, _cx: &mut Context<Self>) {
        use crate::notifications::NotificationSetting;
        let (Some(notifier), Some(call)) = (&self.notifier, call) else { return };
        let (conversation, video) = (call.conversation.clone(), call.video);
        if self.notification_setting == NotificationSetting::Off || self.window_active {
            return;
        }
        let name = self.conversation(&conversation).map(|c| c.title.clone()).unwrap_or_else(|| "Someone".into());
        let title = match self.notification_setting {
            NotificationSetting::NameAndMessage | NotificationSetting::NameOnly => name,
            _ => "Signal".into(),
        };
        let body = match (call.group, video) {
            (true, _) => "Incoming group call",
            (false, true) => "Incoming video call",
            (false, false) => "Incoming voice call",
        };
        notifier.post(&crate::notifications::Notification {
            id: INCOMING_CALL_NOTIFICATION,
            conversation: &conversation,
            title: &title,
            subtitle: None,
            body,
            sound: false,
        });
    }

    pub fn search_in_chat(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        cx.emit(AppEvent::SearchInChat(conversation));
    }

    pub fn jump_to(&mut self, conversation: ConversationId, message_id: i64, cx: &mut Context<Self>) {
        self.jump = Some((conversation.clone(), message_id));
        self.select(Some(conversation.clone()), cx);
        cx.emit(AppEvent::JumpTo { conversation });
    }

    /// The pending jump for `conversation`, if any (taken once).
    pub fn take_jump(&mut self, conversation: &ConversationId) -> Option<i64> {
        let (_, id) = self.jump.take_if(|(c, _)| c == conversation)?;
        Some(id)
    }

    /// Reads our profile, username and link again.
    pub fn reload_own_profile(&mut self, cx: &mut Context<Self>) {
        let Some(aci) = self.account.as_ref().map(|a| a.aci.clone()) else { return };
        let db = self.backend.db().clone();
        let read = self.backend.spawn(async move { signal_backend::own_profile(&db, &aci).await });
        cx.spawn(async move |this, cx| {
            let profile = read.await;
            let _ = this.update(cx, |this, cx| {
                // Our photo is in the people's photos too.
                if let Some(aci) = this.account.as_ref().map(|a| a.aci.clone()) {
                    match &profile.photo {
                        Some(photo) => this.person_avatars.insert(aci, photo.clone()),
                        None => this.person_avatars.remove(&aci),
                    };
                }
                this.own_profile = Some(profile);
                cx.emit(AppEvent::OwnProfileChanged);
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads who has which badge, and the badges, again.
    pub fn reload_badges(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let read = self.backend.spawn(async move {
            Ok::<_, signal_store::Error>((db.badge_holders().await?, db.badges().await?, db.badge_image_files().await?))
        });
        cx.spawn(async move |this, cx| {
            let Ok((holders, badges, files)) = read.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.badges = Badges {
                    holders,
                    badges: badges.into_iter().map(|b| (b.id.clone(), b)).collect(),
                    files: files.into_iter().map(|(url, path)| (url, path.into())).collect(),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Our own service id.
    pub fn our_aci(&self) -> Option<&str> {
        self.account.as_ref().map(|a| a.aci.as_str())
    }

    /// Someone's downloaded profile photo.
    /// Someone's photo: their profile photo, or the one in the phone's
    /// address book, whichever "Use address book photos" puts first (ours
    /// is always our profile photo), as Desktop's `getAvatar`.
    pub fn photo_of(&self, service_id: &str) -> Option<&std::path::Path> {
        let profile = self.person_avatars.get(service_id);
        let contact = self.contact_avatars.get(service_id);
        let ours = self.account.as_ref().is_some_and(|a| a.aci == service_id);
        let prefer_contact = !ours && self.prefs.flag(signal_backend::AccountSetting::PreferContactAvatars);
        let (first, second) = if prefer_contact { (contact, profile) } else { (profile, contact) };
        first.or(second).map(|p| p.as_path())
    }

    /// Desktop's "Import now": asks the phone for its contacts; failed if
    /// they don't come within a minute.
    pub fn sync_contacts(&mut self, cx: &mut Context<Self>) {
        let started = Instant::now();
        self.contact_sync = ContactSync::Pending(started);
        self.send(Command::SyncContacts);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONTACT_SYNC_TIMEOUT).await;
            let _ = this.update(cx, |this, cx| {
                if this.contact_sync == ContactSync::Pending(started) {
                    this.contact_sync = ContactSync::Failed;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// One of this device's on/off settings (Settings).
    pub fn set_toggle(&mut self, toggle: crate::prefs::Toggle, on: bool, cx: &mut Context<Self>) {
        self.prefs.toggles.insert(toggle, on);
        self.save_pref(toggle.key(), on.to_string(), cx);
        // "Play calling sounds" off quiets a call ringing now.
        if toggle == crate::prefs::Toggle::CallSounds && !on {
            self.ringtone = None;
        }
        cx.notify();
    }

    /// Desktop's "Default timer for new chats", for the whole account.
    pub fn set_universal_timer(&mut self, seconds: u32, cx: &mut Context<Self>) {
        self.prefs.universal_timer = seconds;
        self.mark_pending(signal_backend::SETTING_UNIVERSAL_EXPIRE_TIMER, seconds.to_string());
        self.send(Command::SetUniversalExpireTimer(seconds));
        cx.notify();
    }

    /// Desktop's "Who can see my number"; everybody also makes us findable.
    pub fn set_number_shared(&mut self, everybody: bool, cx: &mut Context<Self>) {
        self.prefs.number_shared = everybody;
        self.mark_pending(signal_backend::SETTING_PHONE_NUMBER_SHARING, everybody.to_string());
        if everybody {
            self.prefs.number_discoverable = true;
            self.mark_pending(signal_backend::SETTING_PHONE_NUMBER_DISCOVERABLE, "true".into());
        }
        self.send(Command::SetPhoneNumberSharing(everybody));
        cx.notify();
    }

    /// Desktop's "Who can find me by number".
    pub fn set_number_discoverable(&mut self, discoverable: bool, cx: &mut Context<Self>) {
        self.prefs.number_discoverable = discoverable;
        self.mark_pending(signal_backend::SETTING_PHONE_NUMBER_DISCOVERABLE, discoverable.to_string());
        self.send(Command::SetPhoneNumberDiscoverable(discoverable));
        cx.notify();
    }

    /// Desktop's media auto-download choices (Data usage).
    pub fn set_auto_download(&mut self, choices: signal_backend::auto_download::AutoDownload, cx: &mut Context<Self>) {
        self.prefs.auto_download = choices;
        if let Ok(json) = serde_json::to_string(&choices) {
            self.mark_pending(signal_backend::auto_download::SETTING_AUTO_DOWNLOAD, json);
        }
        self.send(Command::SetAutoDownload(choices));
        cx.notify();
    }

    /// Blocked people and groups (Desktop's Blocked page), people first.
    pub fn blocked(&self) -> Vec<&signal_store::Conversation> {
        let mut blocked: Vec<_> = self.conversations.iter().chain(&self.archived).filter(|c| c.blocked).collect();
        blocked.sort_by_key(|c| (c.kind == signal_store::ConversationKind::Group, c.title.to_lowercase()));
        blocked
    }

    /// Desktop's "Convert typed emoticons to emoji".
    pub fn set_auto_convert_emoji(&mut self, on: bool, cx: &mut Context<Self>) {
        self.prefs.auto_convert_emoji = on;
        self.save_pref(crate::prefs::AUTO_CONVERT_EMOJI_KEY, on.to_string(), cx);
        cx.notify();
    }

    pub fn set_chat(&self, conversation: ConversationId, change: signal_backend::ChatChange) {
        self.send(Command::SetChat { conversation, change });
    }

    pub fn pinned_count(&self) -> usize {
        self.conversations.iter().chain(&self.archived).filter(|c| c.pinned).count()
    }

    pub fn select(&mut self, id: Option<ConversationId>, cx: &mut Context<Self>) {
        if self.selected == id {
            return;
        }
        // Desktop fetches a group as its chat opens (throttled backend-side).
        if let Some(group) = id.as_ref().filter(|id| id.as_group().is_some()) {
            self.send(Command::RefreshGroup { conversation: group.clone() });
        }
        // Opening a chat takes its reaction notifications down.
        if let Some(opened) = &id
            && let Some(ids) = self.reaction_notifications.remove(opened)
            && let Some(notifier) = &self.notifier
        {
            notifier.remove(&ids);
        }
        self.selected = id;
        cx.emit(AppEvent::SelectionChanged);
        cx.notify();
    }

    /// Desktop's previous and next chat shortcuts (⌥↑ and ⌥↓, with ⇧ for
    /// unread chats only): the chat above or below the open one in the
    /// list, wrapping around.
    pub fn select_in_direction(&mut self, up: bool, unread_only: bool, cx: &mut Context<Self>) {
        let listed = self.listed_inbox();
        let unread: Vec<bool> = listed.iter().map(|c| c.unread_count > 0 || c.marked_unread).collect();
        let current = self.selected.as_ref().and_then(|id| listed.iter().position(|c| &c.id == id));
        if let Some(next) = conversation_in_direction(&unread, current, up, unread_only) {
            let id = listed[next].id.clone();
            self.select(Some(id), cx);
        }
    }

    /// Desktop's ⌘1 to ⌘9 (`getConversationAndMessageAtIndex`): the chat
    /// at that place in the list (pinned ones first), or the last one.
    pub fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let listed = self.listed_inbox();
        if let Some(chat) = listed.get(index).or(listed.last()) {
            let id = chat.id.clone();
            self.select(Some(id), cx);
        }
    }

    /// The inbox as the chat list shows it with no search: the selected
    /// folder's chats, and the open chat while it's open (Desktop's
    /// `LeftPaneInboxHelper`), so the shortcuts above move through what's
    /// on screen.
    pub fn listed_inbox(&self) -> Vec<&Conversation> {
        let folder = self.selected_folder();
        self.conversations
            .iter()
            .filter(|c| folder.is_none_or(|f| Self::in_folder(f, c) || self.selected.as_ref() == Some(&c.id)))
            .collect()
    }

    pub fn send(&self, command: Command) {
        self.backend.send(command);
    }

    fn load_emoji_usage(&mut self, cx: &mut Context<Self>) {
        let db = self.backend.db().clone();
        let query = self.backend.spawn(async move { db.emoji_usage().await });
        cx.spawn(async move |this, cx| {
            if let Ok(usage) = query.await {
                let _ = this.update(cx, |this, _| this.emoji_usage = EmojiUsageCache::from_usage(usage));
            }
        })
        .detach();
    }

    /// Records an emoji pick for recents and search ranking.
    pub fn record_emoji(&mut self, glyph: &str) {
        self.emoji_usage.record(glyph);
        let db = self.backend.db().clone();
        let glyph = glyph.to_owned();
        let task = self.backend.spawn(async move { db.record_emoji_use(&glyph).await });
        drop(task); // fire and forget; the spawned task runs regardless
    }

    pub fn setting(&self, key: &'static str, cx: &mut Context<Self>, apply: impl FnOnce(&mut Self, Option<String>, &mut Context<Self>) + 'static) {
        let db = self.backend.db().clone();
        let query = self.backend.spawn(async move { db.setting(key).await });
        cx.spawn(async move |this, cx| {
            let value = query.await.ok().flatten();
            let _ = this.update(cx, |this, cx| apply(this, value, cx));
        })
        .detach();
    }

    pub fn set_setting(&self, key: &'static str, value: String) {
        let db = self.backend.db().clone();
        drop(self.backend.spawn(async move { db.set_setting(key, &value).await }));
    }
}

/// The sticker pack shown in Desktop's `StickerPreviewModal`.
#[derive(Debug, Clone, PartialEq)]
pub enum StickerPreview {
    /// Its stickers are loading (its id).
    Loading(Vec<u8>),
    Ready(Box<signal_backend::stickers::StickerPackPreview>),
    Failed(String),
}

/// A notification's text for a message: its text, or what it holds.
/// What a poll notification says, in Desktop's words: a vote names the
/// sender as the title does ("Alice in Hikers") and quotes the poll's
/// preview (`icu:notificationPollVoteMessage`); an end is the notice's
/// text after 📊 (`icu:PollTerminate--other`).
fn poll_activity_text(sender: &str, group: Option<&str>, question: &str, ended: bool) -> String {
    if ended {
        return signal_store::poll_ended_preview(Some(sender), question);
    }
    let title = match group {
        Some(group) => format!("{sender} in {group}"),
        None => sender.to_owned(),
    };
    format!("{title} voted in the poll \"{}\"", signal_store::poll_preview(question))
}

fn notification_preview(message: &signal_store::Message) -> String {
    // Desktop's "📊 Poll: …" (`getNotificationDataForMessage`).
    if let Some(poll) = &message.poll {
        return signal_store::poll_preview(&poll.question);
    }
    if let Some(view_once) = message.view_once {
        return signal_store::view_once_preview(view_once, message.attachments.first()).into();
    }
    if let Some(content) = &message.content {
        let sender = (message.direction == signal_store::Direction::Incoming)
            .then(|| message.author_name.clone().unwrap_or_else(|| "Unknown contact".into()));
        return signal_store::content_preview(content, sender.as_deref(), None);
    }
    if !message.body.is_empty() {
        // Spoilers stay hidden (Desktop shows "■■■■").
        let body = signal_store::hide_spoilers(&message.body, &message.styles);
        let text: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        return text.chars().take(300).collect();
    }
    match message.attachments.first() {
        Some(a) => signal_store::attachment_preview(a).into(),
        None => "New Message".into(),
    }
}

pub fn new_state(backend: Backend, cx: &mut gpui_kit::App) -> Entity<AppState> {
    cx.new(|cx| AppState::new(backend, cx))
}

/// The incoming call notification's id (one call at a time).
const INCOMING_CALL_NOTIFICATION: &str = "incoming-call";

const RECENT_STICKERS_SETTING: &str = "stickers.recent";
/// Desktop keeps a row or so of recent stickers.
const MAX_RECENT_STICKERS: usize = 8;

/// `hex(pack):id,hex(pack):id`.
fn format_recent_stickers(recents: &[(Vec<u8>, u32)]) -> String {
    recents
        .iter()
        .map(|(pack, id)| format!("{}:{id}", pack.iter().map(|b| format!("{b:02x}")).collect::<String>()))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_recent_stickers(value: &str) -> Vec<(Vec<u8>, u32)> {
    value
        .split(',')
        .filter_map(|item| {
            let (pack, id) = item.split_once(':')?;
            let pack = (0..pack.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(pack.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            Some((pack, id.parse().ok()?))
        })
        .collect()
}


#[cfg(test)]
mod sticker_tests {
    use super::*;

    #[test]
    fn recent_stickers_round_trip() {
        let recents = vec![(vec![0xde, 0x70], 3), (vec![0x01], 12)];
        assert_eq!(parse_recent_stickers(&format_recent_stickers(&recents)), recents);
        assert_eq!(parse_recent_stickers("junk,zz:1,ab:x"), vec![]);
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::conversation_in_direction as find;

    #[test]
    fn chats_up_and_down_as_desktops_tests_have_them() {
        let unread = [false, true, false, true, false];
        // Nothing open: the first or the last.
        assert_eq!(find(&unread, None, false, false), Some(0));
        assert_eq!(find(&unread, None, true, false), Some(4));
        assert_eq!(find(&unread, None, false, true), Some(1));
        assert_eq!(find(&unread, None, true, true), Some(3));
        // Next and previous wrap around.
        assert_eq!(find(&unread, Some(2), false, false), Some(3));
        assert_eq!(find(&unread, Some(4), false, false), Some(0));
        assert_eq!(find(&unread, Some(0), true, false), Some(4));
        // Unread only: the nearest that way, and none past the end.
        assert_eq!(find(&unread, Some(1), false, true), Some(3));
        assert_eq!(find(&unread, Some(3), false, true), None);
        assert_eq!(find(&unread, Some(3), true, true), Some(1));
        assert_eq!(find(&unread, Some(1), true, true), None);
        assert_eq!(find(&[], None, false, false), None);
        assert_eq!(find(&[false, false], Some(0), false, true), None);
    }
}

#[cfg(test)]
mod link_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::TestAppContext;

    use super::*;
    use crate::views::test_support::{demo, wait};

    /// Dana Brooks, in the demo's directory with a made-up number.
    const DANA: &str = "d:00000000-0000-4000-8000-0000000000d5";

    #[gpui_kit::test]
    fn signal_links_open_here_and_others_dont(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        wait(cx, "the account", &mut |cx| state.read_with(cx, |s, _| s.account.is_some()));
        let events = Rc::new(RefCell::new(Vec::new()));
        cx.update(|_, cx| {
            let events = events.clone();
            cx.subscribe(&state, move |_, event: &AppEvent, _| events.borrow_mut().push(format!("{event:?}"))).detach();
        });
        let open = |cx: &mut gpui_kit::VisualTestContext, url: &str| state.update(cx, |s, cx| s.open_signal_link(url, cx));

        // Not Signal's: the caller opens them in the browser.
        assert!(!open(cx, "https://example.com/"));
        assert!(!open(cx, "https://signal.tube/#proxy.example.com"));

        // signal.me by number: the chat opens.
        assert!(open(cx, "https://signal.me/#p/+14155550177"));
        wait(cx, "Dana's chat", &mut |cx| state.read_with(cx, |s, _| s.selected.as_ref().map(|c| c.0.as_str()) == Some(DANA)));

        // A call link's lobby.
        let key: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let url = signal_backend::call_link_url(&key).unwrap().replace("https://", "sgnl://");
        assert!(open(cx, &url));
        wait(cx, "the lobby", &mut |cx| {
            state.read_with(cx, |s, _| s.lobby.as_ref().is_some_and(|l| l.conversation.as_call_link().is_some()))
        });
        state.update(cx, |s, cx| s.close_call_lobby(cx));

        // A sticker pack's link opens the pack (the demo's Garden dots);
        // one that isn't a pack's says so.
        let garden = format!("https://signal.art/addstickers/#pack_id={}&pack_key={}", "9a".repeat(16), "00".repeat(32));
        assert!(open(cx, &garden));
        wait(cx, "the pack", &mut |cx| {
            state.read_with(cx, |s, _| matches!(&s.sticker_preview, Some(StickerPreview::Ready(p)) if p.pack.title == "Garden dots"))
        });
        assert!(open(cx, "sgnl://addstickers/?pack_id=0123&pack_key=ffee"));
        // Links Desktop knows but this app can't use say so.
        assert!(open(cx, "sgnl://linkdevice?uuid=a&pub_key=b"));
        cx.run_until_parked();
        let seen = events.borrow().join("\n");
        assert!(seen.contains("OpenStickerPack"), "{seen}");
        assert!(seen.contains("Error opening sticker pack"), "{seen}");
        assert!(seen.contains("didn't make sense"), "{seen}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn links_from_the_system_wait_for_the_account(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        wait(cx, "the account", &mut |cx| state.read_with(cx, |s, _| s.account.is_some()));
        // As if macOS opened the app with the link: it's queued until the
        // account loads, then opened.
        state.update(cx, |s, cx| {
            let aci = s.account.take().unwrap().aci;
            s.screen = Screen::Loading;
            s.open_link_from_system("sgnl://signal.me/#p/+14155550177".into(), cx);
            s.open_link_from_system("https://example.com/".into(), cx);
            assert_eq!(s.pending_links, ["sgnl://signal.me/#p/+14155550177"]);
            s.handle(signal_backend::Event::Ready { phone: None, aci }, cx);
            assert!(s.pending_links.is_empty());
        });
        wait(cx, "Dana's chat", &mut |cx| state.read_with(cx, |s, _| s.selected.as_ref().map(|c| c.0.as_str()) == Some(DANA)));
        backend.shutdown();
    }
}
