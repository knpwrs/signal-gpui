//! Desktop's story viewer (`StoryViewer`): a person's or group's stories
//! one after another over the window, with a bar per story, each shown for
//! Desktop's time (`getStoryDuration`), then the next person's.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, WindowExt as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyDownEvent, ObjectFit, Pixels,
    RenderImage, SharedString, Subscription, Task, Window, div, img, px,
};
use signal_backend::Command;
use signal_store::{ConversationId, Story, StoryContent};

use crate::state::AppState;
use crate::views::image_viewer::ViewerEvent;
use crate::views::stories::{CANVAS_HEIGHT, CANVAS_WIDTH, render_text_story};

/// How often the progress moves.
const TICK: Duration = Duration::from_millis(50);
/// Desktop's `DEFAULT_DURATION`.
const DEFAULT_SECONDS: f64 = 5.;
/// Desktop's longest story (`MAX_VIDEO_DURATION`).
const MAX_SECONDS: f64 = 30.;
/// Desktop's caption, before "Read more" (`CAPTION_INITIAL_LENGTH`).
const CAPTION_LENGTH: usize = 200;
/// Quick reactions (Desktop's default reaction set).
const REACTIONS: [&str; 6] = ["❤️", "👍", "👎", "😂", "😮", "😢"];

/// Desktop's `MyStories__views`.
pub fn views_text(count: usize) -> String {
    if count == 1 { "1 view".into() } else { format!("{count} views") }
}

/// Desktop's `MyStories__replies`.
fn replies_text(count: usize) -> String {
    if count == 1 { "1 reply".into() } else { format!("{count} replies") }
}

/// The button under our story (Desktop's `StoryViewer`): its views, or
/// "Views off" with view receipts off, and its replies.
pub fn views_button_text(views: usize, replies: usize, receipts: bool) -> String {
    let mut parts = Vec::new();
    if receipts {
        parts.push(views_text(views));
    } else if replies == 0 {
        parts.push("Views off".to_owned());
    }
    if replies > 0 {
        parts.push(replies_text(replies));
    }
    parts.join(" ")
}

/// The tabs of Desktop's `StoryViewsNRepliesModal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewsTab {
    Views,
    Replies,
}

/// Desktop's `getStoryDuration` for text: 5 seconds, a second more for
/// each 15 characters after the first 15, and 2 for a link.
pub fn text_seconds(text: &str, link: bool) -> f64 {
    let count = text.chars().count();
    let extra = count.saturating_sub(15).div_ceil(15);
    DEFAULT_SECONDS + extra as f64 + if link { 2. } else { 0. }
}

/// The queue: whose stories, in the list's order.
struct Section {
    /// `None`: ours.
    chat: Option<ConversationId>,
    title: String,
    stories: Vec<Story>,
}

struct PlayingVideo {
    story: i64,
    player: crate::video_player::VideoPlayer,
    frame: Option<Arc<RenderImage>>,
}

pub struct StoryViewer {
    state: Entity<AppState>,
    sections: Vec<Section>,
    section: usize,
    story: usize,
    /// When the current story (re)started counting, and how far it had got
    /// before a pause.
    started: Instant,
    shown_before: Duration,
    paused: bool,
    video: Option<PlayingVideo>,
    reply: Entity<InputState>,
    /// What the reply box says; set on it when drawn.
    reply_placeholder: SharedString,
    placeholder_shown: SharedString,
    /// Our story's views and replies, open at this tab (the story waits).
    pub(crate) views_open: Option<ViewsTab>,
    focus: FocusHandle,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ViewerEvent> for StoryViewer {}

impl Focusable for StoryViewer {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl StoryViewer {
    /// Opens at `chat`'s stories (`None`: ours).
    pub fn new(state: Entity<AppState>, chat: Option<ConversationId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sections = {
            let s = state.read(cx);
            match &chat {
                None => vec![Section { chat: None, title: "My Stories".into(), stories: s.my_stories() }],
                Some(_) => s
                    .story_groups()
                    .into_iter()
                    .filter(|g| !g.hidden || Some(&g.chat) == chat.as_ref())
                    .map(|g| Section { chat: Some(g.chat), title: g.title, stories: g.stories })
                    .collect(),
            }
        };
        let section = sections.iter().position(|s| s.chat == chat).unwrap_or(0);
        // Start at the first unviewed one, as Desktop does.
        let story = sections.get(section).and_then(|s| s.stories.iter().position(|st| !st.read)).unwrap_or(0);
        let reply = cx.new(|cx| InputState::new(window, cx).placeholder("Reply"));
        let subscriptions = vec![
            cx.subscribe_in(&reply, window, |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Focus => this.set_paused(true, cx),
                // The replies stay open (and the story waiting) after.
                InputEvent::Blur => this.set_paused(this.views_open.is_some(), cx),
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.send_reply(text, None, cx);
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    window.focus(&this.focus, cx);
                }
                _ => {}
            }),
            // Stories download, get viewed and replied to, or are deleted
            // while open.
            cx.observe(&state, |this, state, cx| {
                let fresh = state.read(cx).stories.clone();
                this.refresh(&fresh, cx);
            }),
        ];
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            state,
            sections,
            section,
            story,
            started: Instant::now(),
            shown_before: Duration::ZERO,
            paused: false,
            video: None,
            reply,
            reply_placeholder: "Reply".into(),
            placeholder_shown: "Reply".into(),
            views_open: None,
            focus: cx.focus_handle(),
            _tick: tick,
            _subscriptions: subscriptions,
        };
        this.show(cx);
        this
    }

    /// Takes in the stories as they are now: changed ones updated, deleted
    /// ones gone (the next one shows if it was the one showing).
    fn refresh(&mut self, fresh: &[Story], cx: &mut Context<Self>) {
        let showing = self.current().map(|s| s.id);
        for section in &mut self.sections {
            section.stories.retain(|story| fresh.iter().any(|s| s.id == story.id));
            for story in &mut section.stories {
                if let Some(new) = fresh.iter().find(|s| s.id == story.id) {
                    *story = new.clone();
                }
            }
        }
        let showing_section = self.sections.get(self.section).map(|s| s.chat.clone());
        self.sections.retain(|s| !s.stories.is_empty());
        if self.sections.is_empty() {
            return cx.emit(ViewerEvent::Close);
        }
        if let Some(at) = self.sections.iter().position(|s| Some(&s.chat) == showing_section.as_ref()) {
            self.section = at;
        } else {
            self.section = self.section.min(self.sections.len() - 1);
            self.story = 0;
        }
        let stories = &self.sections[self.section].stories;
        match showing.and_then(|id| stories.iter().position(|s| s.id == id)) {
            Some(at) => self.story = at,
            None => {
                self.story = self.story.min(stories.len() - 1);
                self.show(cx);
            }
        }
        cx.notify();
    }

    /// Desktop's "Delete" for our story, after its confirmation.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(story) = self.current().filter(|s| s.author.is_none()) else { return };
        let (id, state) = (story.id, self.state.clone());
        self.set_paused(true, cx);
        let viewer = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (state, viewer) = (state.clone(), viewer.clone());
            let resume = viewer.clone();
            alert
                .description("Delete this story? It will also be deleted for everyone who received it.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    state.update(cx, |s, cx| s.delete_story(id, cx));
                    let _ = viewer.update(cx, |v, cx| v.set_paused(false, cx));
                    true
                })
                .on_cancel(move |_, _, cx| {
                    let _ = resume.update(cx, |v, cx| v.set_paused(false, cx));
                    true
                })
        });
    }

    fn current(&self) -> Option<&Story> {
        self.sections.get(self.section)?.stories.get(self.story)
    }

    /// The story now showing: viewed, downloaded, its video started.
    fn show(&mut self, cx: &mut Context<Self>) {
        if self.views_open.take().is_some() {
            self.set_paused(false, cx);
        }
        self.started = Instant::now();
        self.shown_before = Duration::ZERO;
        self.video = None;
        let Some(story) = self.current().cloned() else { return };
        let state = self.state.read(cx);
        if story.author.is_some() && !story.read {
            state.send(Command::ViewStory { id: story.id });
        }
        if let StoryContent::Media { attachment, .. } = &story.content
            && attachment.local_path.is_none()
        {
            state.send(Command::FetchStory { id: story.id });
        }
        let placeholder = match (&story.author, story.group.is_some()) {
            (None, _) => "Reply".to_owned(),
            (Some(_), true) => "Reply to Group".to_owned(),
            (Some(_), false) => {
                let name = self.sections.get(self.section).map(|s| s.title.clone()).unwrap_or_default();
                format!("Reply to {}", name.split_whitespace().next().unwrap_or(&name))
            }
        };
        self.reply_placeholder = placeholder.into();
        cx.notify();
    }

    /// How long this story shows (Desktop's `getStoryDuration`); `None`
    /// while that isn't known (a photo or video still downloading).
    fn duration(&self) -> Option<f64> {
        let story = self.current()?;
        match &story.content {
            StoryContent::Text(text) => Some(text_seconds(&text.text, text.preview.is_some())),
            StoryContent::Media { attachment, caption, .. } => {
                attachment.local_path.as_ref()?;
                if attachment.content_type.starts_with("video/") {
                    let player = self.video.as_ref().filter(|v| v.story == story.id)?;
                    let seconds = player.player.duration();
                    (seconds > 0.).then(|| seconds.min(MAX_SECONDS))
                } else if caption.is_empty() {
                    Some(DEFAULT_SECONDS)
                } else {
                    Some(text_seconds(caption, false))
                }
            }
        }
    }

    fn elapsed(&self) -> Duration {
        if self.paused { self.shown_before } else { self.shown_before + self.started.elapsed() }
    }

    fn set_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        if self.paused == paused {
            return;
        }
        if paused {
            self.shown_before += self.started.elapsed();
        } else {
            self.started = Instant::now();
        }
        self.paused = paused;
        if let Some(video) = &self.video {
            if paused { video.player.pause() } else { video.player.play() }
        }
        cx.notify();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.start_video(cx);
        if let Some(video) = &mut self.video
            && let Some(frame) = video.player.frame()
            && let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)
        {
            let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            if let Some(old) = video.frame.replace(image) {
                cx.drop_image(old, None);
            }
        }
        match self.duration() {
            // Not ready: the clock waits (Desktop pauses until it knows).
            None => self.started = Instant::now(),
            Some(seconds) if !self.paused && self.elapsed().as_secs_f64() >= seconds => return self.next(cx),
            Some(_) => {}
        }
        cx.notify();
    }

    /// A downloaded video starts once it's shown.
    fn start_video(&mut self, cx: &mut Context<Self>) {
        let Some(story) = self.current() else { return };
        let StoryContent::Media { attachment, .. } = &story.content else { return };
        if !attachment.content_type.starts_with("video/") || self.video.as_ref().is_some_and(|v| v.story == story.id) {
            return;
        }
        let Some(path) = attachment.local_path.clone() else { return };
        let id = story.id;
        match crate::video_player::VideoPlayer::open(std::path::Path::new(&path)) {
            Ok(player) => {
                if !self.paused {
                    player.play();
                }
                self.started = Instant::now();
                self.shown_before = Duration::ZERO;
                self.video = Some(PlayingVideo { story: id, player, frame: None });
            }
            Err(error) => tracing::warn!(error = %format!("{error:#}"), "couldn't play a story's video"),
        }
        cx.notify();
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        let Some(section) = self.sections.get(self.section) else { return cx.emit(ViewerEvent::Close) };
        if self.story + 1 < section.stories.len() {
            self.story += 1;
        } else if self.section + 1 < self.sections.len() {
            self.section += 1;
            self.story = self.sections[self.section].stories.iter().position(|s| !s.read).unwrap_or(0);
        } else {
            return cx.emit(ViewerEvent::Close);
        }
        self.show(cx);
    }

    fn previous(&mut self, cx: &mut Context<Self>) {
        if self.story > 0 {
            self.story -= 1;
        } else if self.section > 0 {
            self.section -= 1;
            self.story = self.sections[self.section].stories.len().saturating_sub(1);
        }
        self.show(cx);
    }

    fn send_reply(&mut self, text: String, emoji: Option<String>, cx: &mut Context<Self>) {
        let Some(story) = self.current() else { return };
        if text.trim().is_empty() && emoji.is_none() {
            return;
        }
        self.state.read(cx).send(Command::ReplyToStory { id: story.id, body: text, emoji });
    }

    /// Opens (or with `None`, closes) our story's views and replies; the
    /// story waits while they're open.
    pub(crate) fn open_views(&mut self, tab: Option<ViewsTab>, cx: &mut Context<Self>) {
        self.views_open = tab;
        self.set_paused(tab.is_some(), cx);
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" if self.views_open.is_some() => self.open_views(None, cx),
            "escape" => cx.emit(ViewerEvent::Close),
            "right" => self.next(cx),
            "left" => self.previous(cx),
            "space" => self.set_paused(!self.paused, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_story(&self, story: &Story, height: Pixels) -> AnyElement {
        let width = px(f32::from(height) * CANVAS_WIDTH / CANVAS_HEIGHT);
        let frame = div().w(width).h(height).flex_none().rounded(px(12.)).overflow_hidden().bg(gpui_kit::rgb(0x000000));
        match &story.content {
            StoryContent::Text(text) => frame.child(render_text_story(text, height)).into_any_element(),
            StoryContent::Media { attachment, caption, .. } => {
                let picture = match (&attachment.local_path, &self.video) {
                    (Some(_), Some(video)) if video.story == story.id => video
                        .frame
                        .clone()
                        .map(|frame| img(frame).size_full().object_fit(ObjectFit::Contain).into_any_element()),
                    (Some(path), _) if attachment.content_type.starts_with("image/") => Some(
                        img(std::path::PathBuf::from(path)).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                    ),
                    _ => None,
                };
                let caption: String = if caption.chars().count() > CAPTION_LENGTH {
                    caption.chars().take(CAPTION_LENGTH).chain("…".chars()).collect()
                } else {
                    caption.clone()
                };
                frame
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match picture {
                        Some(picture) => picture,
                        None => div().text_color(gpui_kit::rgb(0xffffff)).child("Downloading…").into_any_element(),
                    })
                    .when(!caption.is_empty(), |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .p_4()
                                .bg(Hsla { h: 0., s: 0., l: 0., a: 0.5 })
                                .text_color(gpui_kit::rgb(0xffffff))
                                .text_sm()
                                .child(caption),
                        )
                    })
                    .into_any_element()
            }
        }
    }

    /// Desktop's `StoryViewsNRepliesModal` for our story: who viewed it
    /// (newest first) and its replies, in tabs when there are both.
    fn render_views(&self, story: &Story, tab: ViewsTab, cx: &mut Context<Self>) -> AnyElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let dim = Hsla { a: 0.7, ..white };
        let composer = self.render_group_composer(story, cx);
        let state = self.state.read(cx);
        let receipts = state.story_view_receipts_on();
        let name_of = |sid: &str| {
            state.conversation(&ConversationId::direct(sid)).map(|c| c.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Unknown".into())
        };
        let empty = |text: &'static str| div().py_8().text_center().text_color(dim).child(text).into_any_element();
        let ours = story.author.is_none();
        let views = if !ours {
            None
        } else if !receipts {
            Some(
                div()
                    .py_8()
                    .text_center()
                    .text_color(dim)
                    .child("Enable view receipts to see who’s viewed your stories. Open the Signal app on your mobile device and navigate to Settings > Stories.")
                    .into_any_element(),
            )
        } else if story.views.is_empty() {
            Some(empty("No views yet"))
        } else {
            let mut views = story.views.clone();
            views.sort_by_key(|v| std::cmp::Reverse(v.at));
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(views.into_iter().map(|view| {
                        let name = name_of(&view.viewer);
                        let photo = state.photo_of(&view.viewer).map(std::path::Path::to_path_buf);
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(crate::views::common::person_avatar(
                                photo.as_deref(),
                                &name,
                                crate::theme::color_for(&ConversationId::direct(&view.viewer).0),
                                px(28.),
                            ))
                            .child(div().flex_1().truncate().child(name))
                            .children(view.at.map(|at| div().text_xs().text_color(dim).child(crate::views::common::short_timestamp(at))))
                    }))
                    .into_any_element(),
            )
        };
        let replies = if !story.replies.is_empty() {
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(story.replies.iter().map(|reply| {
                        let who = match &reply.author {
                            None => "You".to_owned(),
                            Some(sid) => reply.author_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| name_of(sid)),
                        };
                        let body = match &reply.emoji {
                            _ if reply.deleted => "This message was deleted.".to_owned(),
                            Some(emoji) => format!("Reacted to the story {emoji}"),
                            None => reply.body.clone(),
                        };
                        let key = format!("story-reply-{}-{}", reply.author.as_deref().unwrap_or("me"), reply.sent_at);
                        let (state, story_id, author, sent_at) = (self.state.clone(), story.id, reply.author.clone(), reply.sent_at);
                        // Desktop's "Delete for everyone" is for ours, in a
                        // group, not yet deleted.
                        let for_everyone = reply.author.is_none() && story.group.is_some() && !reply.deleted;
                        div()
                            .id(SharedString::from(key.clone()))
                            .debug_selector(move || key.clone())
                            .context_menu(move |menu, _, _| {
                                let (state, author) = (state.clone(), author.clone());
                                let (everyone_state, everyone_author) = (state.clone(), author.clone());
                                menu.item(PopupMenuItem::new("Delete for me").on_click(move |_, _, cx| {
                                    let author = author.clone();
                                    state.update(cx, |s, cx| s.delete_story_reply(story_id, author, sent_at, false, cx));
                                }))
                                .when(for_everyone, |menu| {
                                    menu.item(PopupMenuItem::new("Delete for everyone").on_click(move |_, _, cx| {
                                        let author = everyone_author.clone();
                                        everyone_state.update(cx, |s, cx| s.delete_story_reply(story_id, author, sent_at, true, cx));
                                    }))
                                })
                            })
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).truncate().child(who))
                                    .child(div().text_xs().text_color(dim).child(crate::views::common::short_timestamp(reply.sent_at))),
                            )
                            .child(div().text_sm().when(reply.deleted, |d| d.italic().text_color(dim)).child(body))
                    }))
                    .into_any_element(),
            )
        } else if story.group.is_some() {
            // Desktop says so for a group's story only.
            Some(empty("No replies yet"))
        } else {
            None
        };
        let tabs = views.is_some() && replies.is_some();
        let tab = if replies.is_none() { ViewsTab::Views } else if views.is_none() { ViewsTab::Replies } else { tab };
        let tab_button = |id: &'static str, label: &'static str, this: ViewsTab, cx: &mut Context<Self>| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex_1()
                .py_1()
                .text_center()
                .cursor_pointer()
                .border_b_2()
                .border_color(if tab == this { white } else { Hsla { a: 0., ..white } })
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.views_open = Some(this);
                    cx.notify();
                }))
                .child(label)
        };
        let body = match tab {
            ViewsTab::Views => views,
            ViewsTab::Replies => replies,
        };
        div()
            .id("story-views-panel")
            .debug_selector(|| "story-views-panel".into())
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla { h: 0., s: 0., l: 0., a: 0.5 })
            .on_click(cx.listener(|this, _, _, cx| this.open_views(None, cx)))
            .child(
                div()
                    .id("story-views-sheet")
                    .w(px(360.))
                    .max_h(px(480.))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(12.))
                    .bg(gpui_kit::rgb(0x2e2e2e))
                    .text_color(white)
                    // Clicks inside stay inside.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .when(tabs, |d| {
                        d.child(
                            div()
                                .flex()
                                .child(tab_button("story-views-tab", "Views", ViewsTab::Views, cx))
                                .child(tab_button("story-replies-tab", "Replies", ViewsTab::Replies, cx)),
                        )
                    })
                    .child(div().id("story-views-list").flex_1().min_h_0().overflow_y_scroll().children(body))
                    .when(tab == ViewsTab::Replies || !tabs, |d| d.children(composer)),
            )
            .into_any_element()
    }

    /// A group story's reply box (Desktop's `StoryViewsNRepliesModal`
    /// composer), or why there's none.
    fn render_group_composer(&self, story: &Story, cx: &mut Context<Self>) -> Option<AnyElement> {
        let group = story.group.as_ref()?;
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let member = self.state.read(cx).conversation(group).is_some_and(|g| !g.left);
        if !member {
            return Some(
                div()
                    .text_sm()
                    .text_color(Hsla { a: 0.7, ..white })
                    .child("You can\u{2019}t reply to this story because you\u{2019}re no longer a member of this group.")
                    .into_any_element(),
            );
        }
        if !story.allows_replies {
            return None;
        }
        Some(
            div()
                .id("story-group-composer")
                .debug_selector(|| "story-group-composer".into())
                .flex()
                .flex_col()
                .gap_2()
                .child(div().flex().gap_2().children(REACTIONS.iter().map(|emoji| {
                    let emoji = *emoji;
                    let key = format!("story-group-react-{emoji}");
                    div()
                        .id(SharedString::from(key.clone()))
                        .debug_selector(move || key.clone())
                        .cursor_pointer()
                        .text_lg()
                        .on_click(cx.listener(move |this, _, _, cx| this.send_reply(String::new(), Some(emoji.into()), cx)))
                        .child(emoji)
                })))
                .child(Input::new(&self.reply))
                .into_any_element(),
        )
    }
}

impl gpui_kit::Render for StoryViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let placeholder = self.reply_placeholder.clone();
        if self.placeholder_shown != placeholder {
            self.placeholder_shown = placeholder.clone();
            self.reply.update(cx, |input, cx| input.set_placeholder(placeholder, window, cx));
        }
        let Some(section) = self.sections.get(self.section) else {
            return div().id("story-viewer").into_any_element();
        };
        let Some(story) = section.stories.get(self.story).cloned() else {
            return div().id("story-viewer").into_any_element();
        };
        let title = section.title.clone();
        let count = section.stories.len();
        let progress = self.duration().map_or(0., |d| (self.elapsed().as_secs_f64() / d).clamp(0., 1.)) as f32;
        let height = (window.viewport_size().height - px(150.)).max(px(200.));
        let width = px(f32::from(height) * CANVAS_WIDTH / CANVAS_HEIGHT);
        // Desktop: "{name} to {group}" for a group's story.
        let name = match (&story.author, &story.group) {
            (None, _) => "You".to_owned(),
            (Some(_), Some(_)) => format!("{} to {title}", story.author_name.clone().unwrap_or_default()),
            (Some(_), None) => title.clone(),
        };
        let theirs = story.author.is_some();
        let one_to_one = theirs && story.group.is_none() && story.allows_replies;
        let segments = (0..count).map(|ix| {
            let fill = if ix < self.story { 1. } else if ix == self.story { progress } else { 0. };
            div()
                .flex_1()
                .h(px(2.))
                .rounded_full()
                .bg(Hsla { a: 0.4, ..white })
                .child(div().h_full().w(gpui_kit::relative(fill)).rounded_full().bg(white))
        });
        let button = |id: &'static str, icon: IconName| {
            div()
                .id(id)
                .size(px(36.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .hover(|d| d.bg(Hsla { a: 0.15, ..white }))
                .child(Icon::new(icon).size_5().text_color(white))
        };
        div()
            .id("story-viewer")
            .debug_selector(|| "story-viewer".into())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(Hsla { h: 0., s: 0., l: 0., a: 0.96 })
            .text_color(white)
            .child(
                // The header, as wide as the story.
                div()
                    .w(width)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().flex().gap(px(1.)).children(segments))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(name))
                            .child(div().text_sm().opacity(0.8).child(crate::views::common::short_timestamp(story.sent_at)))
                            .when(!theirs, |d| {
                                d.child(
                                    button("story-delete", IconName::Trash)
                                        .debug_selector(|| "story-delete".into())
                                        .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
                                )
                            })
                            .child(
                                button("story-pause", if self.paused { IconName::Play } else { IconName::Pause })
                                    .on_click(cx.listener(|this, _, _, cx| this.set_paused(!this.paused, cx))),
                            )
                            .child(button("story-close", IconName::X).on_click(cx.listener(|_, _, _, cx| cx.emit(ViewerEvent::Close)))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(button("story-previous", IconName::ChevronLeft).on_click(cx.listener(|this, _, _, cx| this.previous(cx))))
                    .child(
                        div()
                            .id("story-canvas")
                            .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| this.set_paused(true, cx)))
                            .on_mouse_up(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| this.set_paused(false, cx)))
                            .child(self.render_story(&story, height)),
                    )
                    .child(button("story-next", IconName::ChevronRight).on_click(cx.listener(|this, _, _, cx| this.next(cx)))),
            )
            .child(
                div()
                    .w(width)
                    .flex()
                    .flex_col()
                    .gap_2()
                    // Ours: Desktop's "n views" / "n replies", opening
                    // who viewed and replied.
                    .when(!theirs, |d| {
                        let receipts = self.state.read(cx).story_view_receipts_on();
                        let label = views_button_text(story.views.len(), story.replies.len(), receipts);
                        let tab = if receipts || story.replies.is_empty() { ViewsTab::Views } else { ViewsTab::Replies };
                        d.child(
                            div()
                                .id("story-views")
                                .debug_selector(|| "story-views".into())
                                .self_center()
                                .px_3()
                                .py_1()
                                .rounded_full()
                                .cursor_pointer()
                                .text_sm()
                                .hover(|d| d.bg(Hsla { a: 0.15, ..white }))
                                .on_click(cx.listener(move |this, _, _, cx| this.open_views(Some(tab), cx)))
                                .child(label),
                        )
                    })
                    // A group's: Desktop's "Reply to Group", opening its
                    // replies and the reply box.
                    .when(theirs && story.group.is_some(), |d| {
                        let label = if story.replies.is_empty() {
                            "Reply to Group".to_owned()
                        } else {
                            format!("Reply to Group · {}", replies_text(story.replies.len()))
                        };
                        d.child(
                            div()
                                .id("story-group-replies")
                                .debug_selector(|| "story-group-replies".into())
                                .self_center()
                                .px_3()
                                .py_1()
                                .rounded_full()
                                .cursor_pointer()
                                .text_sm()
                                .hover(|d| d.bg(Hsla { a: 0.15, ..white }))
                                .on_click(cx.listener(|this, _, _, cx| this.open_views(Some(ViewsTab::Replies), cx)))
                                .child(label),
                        )
                    })
                    .when(one_to_one, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&self.reply)))
                                .children(REACTIONS.iter().map(|emoji| {
                                    let emoji = *emoji;
                                    div()
                                        .id(SharedString::from(format!("story-react-{emoji}")))
                                        .cursor_pointer()
                                        .text_lg()
                                        .on_click(cx.listener(move |this, _, _, cx| this.send_reply(String::new(), Some(emoji.into()), cx)))
                                        .child(emoji)
                                })),
                        )
                    }),
            )
            .when_some(self.views_open, |d, tab| d.child(self.render_views(&story, tab, cx)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn text_stories_show_for_desktops_time() {
        use super::text_seconds;
        assert_eq!(text_seconds("short", false), 5.);
        assert_eq!(text_seconds(&"x".repeat(16), false), 6.);
        assert_eq!(text_seconds(&"x".repeat(45), false), 7.);
        assert_eq!(text_seconds("short", true), 7., "a link adds two");
    }

    #[test]
    fn our_stories_say_views_and_replies_as_desktop_does() {
        use super::views_button_text;
        assert_eq!(views_button_text(0, 0, true), "0 views");
        assert_eq!(views_button_text(1, 1, true), "1 view 1 reply");
        assert_eq!(views_button_text(3, 0, false), "Views off");
        assert_eq!(views_button_text(3, 2, false), "2 replies", "replies take the place of Views off");
    }
}
