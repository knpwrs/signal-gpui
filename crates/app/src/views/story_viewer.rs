//! Desktop's story viewer (`StoryViewer`): a person's or group's stories
//! one after another over the window, with a bar per story, each shown for
//! Desktop's time (`getStoryDuration`), then the next person's.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
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
                InputEvent::Blur => this.set_paused(false, cx),
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.send_reply(text, None, cx);
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    window.focus(&this.focus, cx);
                }
                _ => {}
            }),
            // Stories download and get viewed while open.
            cx.observe(&state, |this, state, cx| {
                let fresh = state.read(cx).stories.clone();
                for section in &mut this.sections {
                    for story in &mut section.stories {
                        if let Some(new) = fresh.iter().find(|s| s.id == story.id) {
                            *story = new.clone();
                        }
                    }
                }
                cx.notify();
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
            focus: cx.focus_handle(),
            _tick: tick,
            _subscriptions: subscriptions,
        };
        this.show(cx);
        this
    }

    fn current(&self) -> Option<&Story> {
        self.sections.get(self.section)?.stories.get(self.story)
    }

    /// The story now showing: viewed, downloaded, its video started.
    fn show(&mut self, cx: &mut Context<Self>) {
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

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
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
                    // Ours: who's seen it (Desktop's "n views").
                    .when(!theirs, |d| {
                        let views = story.views.len();
                        d.child(div().text_sm().text_center().child(if views == 1 { "1 view".to_owned() } else { format!("{views} views") }))
                    })
                    // A group's: its replies.
                    .when(theirs && story.group.is_some(), |d| {
                        d.children(story.replies.iter().rev().take(3).rev().map(|reply| {
                            let who = reply.author_name.clone().unwrap_or_else(|| if reply.author.is_none() { "You".into() } else { String::new() });
                            div()
                                .text_sm()
                                .child(format!("{who}: {}", if reply.emoji.is_some() { format!("Reacted {}", reply.body) } else { reply.body.clone() }))
                        }))
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
}
