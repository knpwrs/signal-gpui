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
/// Settings key: chats whose stories are hidden (JSON conversation ids).
const HIDDEN_STORIES_SETTING: &str = "stories_hidden";

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
    /// A link preview the composer asked for (`None`: nothing to show).
    LinkPreview { url: String, preview: Option<std::rc::Rc<signal_backend::LinkPreviewDraft>> },
    /// How the sticker pack creator's upload is going.
    StickerUpload(signal_backend::sticker_upload::StickerUpload),
}

/// The audio attachment being played.
pub struct AudioPlaying {
    pub message_id: i64,
    pub index: usize,
    /// Who sent it ("You" for ours), for the mini player.
    pub title: String,
    player: crate::audio::Player,
    pub rate: f32,
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
    pub selected: Option<ConversationId>,
    /// Authors currently typing, per conversation, with expiry.
    typing: HashMap<ConversationId, Vec<(String, Instant)>>,
    pub emoji_usage: EmojiUsageCache,
    pub last_error: Option<String>,
    /// Installed sticker packs (refreshed when the picker opens).
    pub sticker_packs: Vec<std::sync::Arc<signal_backend::StickerPackView>>,
    /// Whether a Giphy API key is saved (GIF search needs one).
    pub has_giphy_key: bool,
    /// Decoded looping videos (GIFs), by file and width.
    loops: LoopCache<std::sync::Arc<gpui_kit::RenderImage>>,
    /// The audio attachment or voice message playing (or paused), as
    /// Desktop plays one at a time, whichever chat is open.
    pub audio: Option<AudioPlaying>,
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
    /// Story view receipts, if set (unset follows read receipts).
    pub story_view_receipts: Option<bool>,
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
    /// Our microphone and camera in the call.
    pub call_audio: bool,
    pub call_video: bool,
    /// The call screen is minimized to a small window.
    pub call_minimized: bool,
    /// Screens and windows to present (loaded for the picker).
    pub share_sources: Option<Result<Vec<signal_backend::ShareSource>, String>>,
    /// Microphones, speakers and cameras, for Settings (loaded on opening it).
    pub call_devices: Option<signal_backend::CallDevices>,
    /// Ringing for an incoming call.
    ringtone: Option<crate::ringtone::Ringtone>,
    /// The safety number last loaded (`Command::LoadSafetyNumber`).
    pub safety_number: Option<(ConversationId, Result<signal_backend::SafetyNumber, String>)>,
    pub catching_up: Option<CatchUp>,
    /// Like Desktop, notifications wait until the catch-up is done, and then
    /// only the latest is shown.
    held_notification: Option<(ConversationId, i64, bool)>,
}

impl EventEmitter<AppEvent> for AppState {}

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
            jump: None,
            selected: None,
            typing: HashMap::new(),
            emoji_usage: EmojiUsageCache::default(),
            last_error: None,
            image_optimization: Default::default(),
            sticker_packs: Vec::new(),
            has_giphy_key: std::env::var_os("GIPHY_API_KEY").is_some(),
            loops: Default::default(),
            audio: None,
            start_recorder: crate::recorder::start_microphone,
            megaphones: Vec::new(),
            tab: NavTab::default(),
            nav_collapsed: false,
            stories: Vec::new(),
            hidden_stories: Default::default(),
            stories_disabled: false,
            story_view_receipts: None,
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
            call: None,
            lobby: None,
            call_audio: true,
            call_video: false,
            call_minimized: false,
            ringtone: None,
            call_devices: None,
            share_sources: None,
            catching_up: None,
            held_notification: None,
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
    /// blocked, or it's muted (a mention still notifies, as on Desktop).
    fn maybe_notify(&mut self, conversation: ConversationId, message_id: i64, mentions_me: bool, cx: &mut Context<Self>) {
        if self.catching_up.is_some() {
            self.held_notification = Some((conversation, message_id, mentions_me));
            return;
        }
        let setting = self.notification_setting;
        if self.notifier.is_none() || setting == crate::notifications::NotificationSetting::Off {
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
            if chat.blocked || (chat.muted && !mentions_me) {
                return;
            }
            let _ = this.update(cx, |this, _| {
                let Some(notifier) = this.notifier.clone() else { return };
                let sender = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
                let group = (chat.kind == signal_store::ConversationKind::Group).then_some(chat.title.as_str());
                let sender = if group.is_some() { sender } else { chat.title.clone() };
                let (title, body) = setting.text(&sender, group, &notification_preview(&message));
                let id = format!("{}:{message_id}", chat.id.0);
                notifier.post(&crate::notifications::Notification {
                    id: &id,
                    conversation: &chat.id,
                    title: &title,
                    subtitle: None,
                    body: &body,
                });
                this.shown_notifications.entry(chat.id.clone()).or_default().push(id);
            });
        })
        .detach();
    }

    /// Takes down notifications for chats that have been read (here or on
    /// another device) and updates the Dock's unread count.
    fn sync_notifications(&mut self) {
        let Some(notifier) = self.notifier.clone() else { return };
        let all = || self.conversations.iter().chain(&self.archived);
        let read: Vec<ConversationId> = all()
            .filter(|c| c.unread_count == 0 && self.shown_notifications.contains_key(&c.id))
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
            && let Some((conversation, message_id, mentions_me)) = self.held_notification.take()
        {
            self.maybe_notify(conversation, message_id, mentions_me, cx);
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
                self.start_catch_up(cx);
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
            Event::SafetyNumber { conversation, result } => self.safety_number = Some((conversation, result)),
            Event::Call(update) => self.on_call(update, cx),
            Event::CallDevices(devices) => self.call_devices = Some(devices),
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
            Event::GroupCreated(conversation) => {
                self.reload_conversations(cx);
                self.select(Some(conversation), cx);
            }
            Event::Incoming { conversation, message_id, mentions_me } => {
                self.maybe_notify(conversation, message_id, mentions_me, cx)
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
            }
            Event::StickerUpload(upload) => cx.emit(AppEvent::StickerUpload(upload)),
            Event::RecipientFound { query, result } => cx.emit(AppEvent::RecipientFound { query, result }),
            Event::LinkPreview { url, preview } => {
                cx.emit(AppEvent::LinkPreview { url, preview: preview.map(std::rc::Rc::new) })
            }
            Event::AuthorNamesChanged(names) => cx.emit(AppEvent::AuthorNamesChanged(names.into())),
            Event::Error(error) => {
                tracing::warn!("backend error: {error}");
                self.report_error(error, cx);
            }
            Event::Unlinked => {
                // The old account's notifications and badge go too.
                if let Some(notifier) = self.notifier.clone() {
                    let mut shown: Vec<String> = self.shown_notifications.drain().flat_map(|(_, ids)| ids).collect();
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
                self.ringtone = None;
                self.conversations.clear();
                self.archived.clear();
                self.person_avatars.clear();
                self.selected = None;
                // The database was cleared; so are the next account's caches.
                self.typing.clear();
                self.emoji_usage = EmojiUsageCache::default();
                self.recent_stickers.clear();
                self.sticker_packs.clear();
                self.has_giphy_key = std::env::var_os("GIPHY_API_KEY").is_some();
                self.image_optimization = Default::default();
                self.history = None;
                self.catching_up = None;
                self.held_notification = None;
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
                ))
            });
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((conversations, archived, avatars)) => {
                        this.conversations = conversations;
                        this.archived = archived;
                        this.person_avatars = avatars.into_iter().map(|(sid, path)| (sid, path.into())).collect();
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
            ))
        });
        cx.spawn(async move |this, cx| {
            let (optimization, recents, giphy, notifications, collapsed, hidden, stories_off, view_receipts) =
                read.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.nav_collapsed = collapsed.as_deref() == Some("true");
                this.hidden_stories = hidden
                    .and_then(|h| serde_json::from_str::<Vec<String>>(&h).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(ConversationId)
                    .collect();
                this.stories_disabled = stories_off.as_deref() == Some("true");
                this.story_view_receipts = view_receipts.map(|v| v != "false");
                this.notification_setting = crate::notifications::NotificationSetting::from_setting(notifications.as_deref());
                this.has_giphy_key |= giphy.is_some_and(|k| !k.trim().is_empty());
                this.image_optimization = signal_backend::media::Optimization::from_setting(optimization.as_deref());
                this.recent_stickers = recents.as_deref().map(parse_recent_stickers).unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
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

    pub fn set_tab(&mut self, tab: NavTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
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
    pub fn set_story_hidden(&mut self, chat: ConversationId, hidden: bool, cx: &mut Context<Self>) {
        let changed = if hidden { self.hidden_stories.insert(chat) } else { self.hidden_stories.remove(&chat) };
        if changed {
            let mut ids: Vec<&str> = self.hidden_stories.iter().map(|c| c.0.as_str()).collect();
            ids.sort();
            self.save_setting(HIDDEN_STORIES_SETTING, serde_json::to_string(&ids).unwrap_or_default(), cx);
            cx.notify();
        }
    }

    /// Desktop's "Turn off stories" / "Turn on stories": off, none are kept
    /// or shown, and the tab goes.
    pub fn set_stories_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.stories_disabled = disabled;
        if disabled && self.tab == NavTab::Stories {
            self.tab = NavTab::Chats;
        }
        self.save_setting(signal_backend::SETTING_STORIES_DISABLED, disabled.to_string(), cx);
        if disabled {
            self.stories.clear();
            let db = self.backend.db().clone();
            let remove = self.backend.spawn(async move { db.remove_all_stories().await });
            cx.background_spawn(async move {
                if let Ok(files) = remove.await {
                    for file in files {
                        let _ = std::fs::remove_file(file);
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }

    /// Whether we tell people we viewed their stories (and see who viewed
    /// ours); unset follows read receipts, as in Desktop.
    pub fn set_story_view_receipts(&mut self, on: bool, cx: &mut Context<Self>) {
        self.story_view_receipts = Some(on);
        self.save_setting(signal_backend::SETTING_STORY_VIEW_RECEIPTS, on.to_string(), cx);
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

    /// Sends a sticker and remembers it as recently used.
    pub fn send_sticker(
        &mut self,
        conversation: ConversationId,
        pack: std::sync::Arc<signal_backend::StickerPackView>,
        sticker: signal_backend::StickerView,
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
        self.send(Command::SendSticker { conversation, pack, sticker });
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
        // The one playing stops first.
        self.audio = None;
        let player = match crate::audio::Player::open(path) {
            Ok(player) => player,
            Err(error) => return self.report_error(format!("Couldn't play it: {error:#}"), cx),
        };
        if fraction > 0. {
            player.seek(fraction * player.duration());
        }
        player.play();
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
                        this.audio = None;
                        cx.notify();
                    }
                    ended
                });
                if ended.unwrap_or(true) {
                    break;
                }
            }
        });
        self.audio = Some(AudioPlaying { message_id, index, title, player, rate: 1., last_position: 0., _poll: poll });
        cx.notify();
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
        if conversation.as_group().is_some() {
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
            if self.notification_setting != crate::notifications::NotificationSetting::Off {
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
        }
        self.call = update;
    }

    /// Desktop's incoming call notification ("Incoming voice call"), which
    /// goes once the call is answered or over.
    fn notify_incoming_call(&self, call: Option<&signal_backend::CallUpdate>, _cx: &mut Context<Self>) {
        use crate::notifications::NotificationSetting;
        let (Some(notifier), Some(call)) = (&self.notifier, call) else { return };
        let (conversation, video) = (call.conversation.clone(), call.video);
        if self.notification_setting == NotificationSetting::Off {
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

    /// Someone's downloaded profile photo.
    pub fn photo_of(&self, service_id: &str) -> Option<&std::path::Path> {
        self.person_avatars.get(service_id).map(|p| p.as_path())
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
        self.selected = id;
        cx.emit(AppEvent::SelectionChanged);
        cx.notify();
    }

    /// Selects the conversation `delta` rows away from the current one.
    pub fn select_relative(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.conversations.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.conversations.iter().position(|c| &c.id == id));
        let next = match current {
            Some(ix) => (ix as isize + delta).clamp(0, self.conversations.len() as isize - 1) as usize,
            None => 0,
        };
        let id = self.conversations[next].id.clone();
        self.select(Some(id), cx);
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

/// A notification's text for a message: its text, or what it holds.
fn notification_preview(message: &signal_store::Message) -> String {
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
