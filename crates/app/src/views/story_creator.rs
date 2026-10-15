//! Desktop's story creator (`StoryCreator`, `TextStoryCreator`,
//! `SendStoryModal`): a text story with Desktop's styles and backgrounds,
//! or a photo or video with a caption; then where to send it.

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyDownEvent, ObjectFit, Render,
    SharedString, Subscription, Window, div, img, px,
};
use signal_backend::{Command, OutgoingStory, StoryTarget};
use signal_store::{ConversationId, ConversationKind, StoryBackground, StoryFont, TextStory};

use crate::state::AppState;
use crate::views::image_viewer::ViewerEvent;
use crate::views::stories::{CANVAS_HEIGHT, CANVAS_WIDTH, render_text_story};

/// Desktop's longest story video (`isVideoGoodForStories`: 30.999 s).
pub const MAX_VIDEO_SECONDS: f64 = 30.999;

/// Desktop's text story backgrounds, BG1 to BG8 (`TextStoryCreator`).
fn backgrounds() -> Vec<StoryBackground> {
    let gradient = |colors: [u32; 4]| StoryBackground::Gradient {
        colors: colors.to_vec(),
        positions: vec![0., 0.33, 0.66, 1.],
        angle: 180,
    };
    vec![
        StoryBackground::Color(4285041620),
        StoryBackground::Color(4287006657),
        StoryBackground::Color(4290019212),
        StoryBackground::Color(4287205768),
        StoryBackground::Color(4283667331),
        gradient([0xff19a9fa, 0xff7097d7, 0xffd1998d, 0xffffc369]),
        gradient([0xff4437d8, 0xff6b70de, 0xffb774e0, 0xffff8e8e]),
        gradient([0xff004044, 0xff2c5f45, 0xff648e52, 0xff93b864]),
    ]
}

/// Desktop's style choices, in its order.
const FONTS: [(StoryFont, &str); 5] = [
    (StoryFont::Regular, "Regular"),
    (StoryFont::Bold, "Bold"),
    (StoryFont::Serif, "Serif"),
    (StoryFont::Script, "Script"),
    (StoryFont::Condensed, "Condensed"),
];

/// Desktop's text background: none, a white pill, or the inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextBackground {
    None,
    Background,
    Inverse,
}

/// An MP4's length in seconds, from its movie header (`mvhd`), as Desktop
/// reads it before posting a video.
pub fn mp4_seconds(bytes: &[u8]) -> Option<f64> {
    let at = bytes.windows(4).position(|w| w == b"mvhd")? + 4;
    let version = *bytes.get(at)?;
    let read = |from: usize, len: usize| -> Option<u64> {
        let slice = bytes.get(from..from + len)?;
        Some(slice.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    };
    let (timescale, duration) = if version == 1 {
        (read(at + 4 + 16, 4)?, read(at + 4 + 20, 8)?)
    } else {
        (read(at + 4 + 8, 4)?, read(at + 4 + 12, 4)?)
    };
    (timescale > 0).then(|| duration as f64 / timescale as f64)
}

/// Why a picked file can't be a story (Desktop's words), if it can't.
pub fn story_file_problem(bytes: &[u8], content_type: &str) -> Option<&'static str> {
    if content_type.starts_with("image/") {
        return None;
    }
    if content_type != "video/mp4" {
        return Some("Cannot post video to story as it is an unsupported file format");
    }
    match mp4_seconds(bytes) {
        Some(seconds) if seconds <= MAX_VIDEO_SECONDS => None,
        Some(_) => Some("Cannot post video to story as it is too long"),
        None => Some("Cannot post video to story as it is an unsupported file format"),
    }
}

/// What's being made.
enum Draft {
    Text,
    Media { bytes: Arc<Vec<u8>>, content_type: String, path: std::path::PathBuf },
}

pub struct StoryCreator {
    state: Entity<AppState>,
    draft: Draft,
    /// Choosing where to send it.
    pub(crate) choosing: bool,
    pub(crate) text: Entity<InputState>,
    caption: Entity<InputState>,
    font: usize,
    text_background: TextBackground,
    background: usize,
    targets: BTreeSet<Target>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    MyStory,
    Group(String),
}

impl EventEmitter<ViewerEvent> for StoryCreator {}

impl Focusable for StoryCreator {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl StoryCreator {
    /// A text story.
    pub fn text(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(state, Draft::Text, window, cx)
    }

    /// A photo or video (already checked with [`story_file_problem`]).
    pub fn media(
        state: Entity<AppState>,
        bytes: Arc<Vec<u8>>,
        content_type: String,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(state, Draft::Media { bytes, content_type, path }, window, cx)
    }

    fn new(state: Entity<AppState>, draft: Draft, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| InputState::new(window, cx).placeholder("Add text"));
        let caption = cx.new(|cx| InputState::new(window, cx).placeholder("Add a message"));
        let subscriptions = vec![
            cx.subscribe(&text, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.subscribe(&caption, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        let mut targets = BTreeSet::new();
        targets.insert(Target::MyStory);
        Self {
            state,
            draft,
            choosing: false,
            text,
            caption,
            font: 0,
            text_background: TextBackground::None,
            background: 0,
            targets,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn text_story(&self, cx: &gpui_kit::App) -> TextStory {
        let (foreground, text_background) = match self.text_background {
            TextBackground::None => (None, None),
            // A white pill, black text.
            TextBackground::Background => (Some(0xff00_0000), Some(0xffff_ffff)),
            // A black pill, white text.
            TextBackground::Inverse => (Some(0xffff_ffff), Some(0xff00_0000)),
        };
        TextStory {
            text: self.text.read(cx).value().to_string(),
            font: FONTS[self.font].0,
            foreground,
            text_background,
            background: backgrounds()[self.background].clone(),
            preview: None,
        }
    }

    fn ready(&self, cx: &gpui_kit::App) -> bool {
        match &self.draft {
            Draft::Text => !self.text.read(cx).value().trim().is_empty(),
            Draft::Media { .. } => true,
        }
    }

    pub(crate) fn send(&mut self, cx: &mut Context<Self>) {
        let story = match &self.draft {
            Draft::Text => OutgoingStory::Text(Box::new(self.text_story(cx))),
            Draft::Media { bytes, content_type, .. } => OutgoingStory::Media {
                bytes: bytes.clone(),
                content_type: content_type.clone(),
                caption: self.caption.read(cx).value().trim().to_owned(),
            },
        };
        let targets: Vec<StoryTarget> = self
            .targets
            .iter()
            .map(|t| match t {
                Target::MyStory => StoryTarget::MyStory,
                Target::Group(id) => StoryTarget::Group(ConversationId(id.clone())),
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        self.state.read(cx).send(Command::PostStory { story, targets });
        cx.emit(ViewerEvent::Close);
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            if self.choosing {
                self.choosing = false;
                cx.notify();
            } else {
                cx.emit(ViewerEvent::Close);
            }
            cx.stop_propagation();
        }
    }

    fn render_editor(&self, height: gpui_kit::Pixels, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let width = px(f32::from(height) * CANVAS_WIDTH / CANVAS_HEIGHT);
        let preview = match &self.draft {
            Draft::Text => {
                let mut story = self.text_story(cx);
                if story.text.trim().is_empty() {
                    story.text = "Add text".into();
                }
                render_text_story(&story, height)
            }
            Draft::Media { content_type, path, .. } => {
                let frame = div().w(width).h(height).flex().items_center().justify_center().bg(gpui_kit::rgb(0x000000));
                if content_type.starts_with("image/") {
                    frame.child(img(path.clone()).size_full().object_fit(ObjectFit::Contain)).into_any_element()
                } else {
                    frame.text_color(white).child("Video").into_any_element()
                }
            }
        };
        let control = |id: &'static str, label: String| {
            Button::new(id).label(label).small().outline()
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(div().rounded(px(12.)).overflow_hidden().child(preview))
            .child(match &self.draft {
                Draft::Text => div()
                    .w(width)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&self.text))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .child(control("story-font", FONTS[self.font].1.to_owned()).on_click(cx.listener(|this, _, _, cx| {
                                this.font = (this.font + 1) % FONTS.len();
                                cx.notify();
                            })))
                            .child(
                                control(
                                    "story-text-background",
                                    match self.text_background {
                                        TextBackground::None => "No text background",
                                        TextBackground::Background => "Text background",
                                        TextBackground::Inverse => "Inverse",
                                    }
                                    .to_owned(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.text_background = match this.text_background {
                                        TextBackground::None => TextBackground::Background,
                                        TextBackground::Background => TextBackground::Inverse,
                                        TextBackground::Inverse => TextBackground::None,
                                    };
                                    cx.notify();
                                })),
                            )
                            .child(control("story-background", "Background".into()).on_click(cx.listener(|this, _, _, cx| {
                                this.background = (this.background + 1) % backgrounds().len();
                                cx.notify();
                            }))),
                    )
                    .into_any_element(),
                Draft::Media { .. } => div().w(width).child(Input::new(&self.caption)).into_any_element(),
            })
            .into_any_element()
    }

    fn render_targets(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let groups: Vec<(String, String)> = {
            let state = self.state.read(cx);
            state
                .conversations
                .iter()
                .chain(state.archived.iter())
                .filter(|c| c.kind == ConversationKind::Group && !c.left && !c.blocked)
                .map(|c| (c.id.0.clone(), c.title.clone()))
                .collect()
        };
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let row = |target: Target, label: String, cx: &mut Context<Self>| {
            let on = self.targets.contains(&target);
            let key = match &target {
                Target::MyStory => "story-target-my-story".to_owned(),
                Target::Group(id) => format!("story-target-{id}"),
            };
            div()
                .id(SharedString::from(key.clone()))
                .debug_selector(move || key.clone())
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|d| d.bg(Hsla { a: 0.1, ..white }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.targets.remove(&target) {
                        this.targets.insert(target.clone());
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .size(px(18.))
                        .flex_none()
                        .rounded(px(4.))
                        .border_2()
                        .border_color(white)
                        .when(on, |d| d.bg(white)),
                )
                .child(div().flex_1().truncate().child(label))
        };
        let mut rows = vec![row(Target::MyStory, "My Story".into(), cx).into_any_element()];
        for (id, title) in groups {
            rows.push(row(Target::Group(id), title, cx).into_any_element());
        }
        div()
            .w(px(360.))
            .max_h(px(480.))
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Send to"))
            .child(div().id("story-targets").flex().flex_col().overflow_y_scroll().children(rows))
            .into_any_element()
    }
}

impl Render for StoryCreator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let height = (window.viewport_size().height - px(260.)).clamp(px(200.), px(720.));
        let ready = self.ready(cx);
        let choosing = self.choosing;
        let any_target = !self.targets.is_empty();
        div()
            .id("story-creator")
            .debug_selector(|| "story-creator".into())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(Hsla { h: 0., s: 0., l: 0., a: 0.96 })
            .text_color(white)
            .child(if choosing { self.render_targets(cx) } else { self.render_editor(height, cx) })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("story-cancel").label("Cancel").ghost().on_click(cx.listener(|this, _, _, cx| {
                        if this.choosing {
                            this.choosing = false;
                            cx.notify();
                        } else {
                            cx.emit(ViewerEvent::Close);
                        }
                    })))
                    .child(if choosing {
                        Button::new("story-send")
                            .label("Send story")
                            .primary()
                            .disabled(!any_target)
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
                    } else {
                        Button::new("story-next").label("Next").primary().disabled(!ready).on_click(cx.listener(|this, _, _, cx| {
                            this.choosing = true;
                            cx.notify();
                        }))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4(version: u8, timescale: u32, duration: u64) -> Vec<u8> {
        let mut out = b"\0\0\0\x20ftypisom".to_vec();
        out.extend_from_slice(b"\0\0\0\x6cmvhd");
        out.push(version);
        out.extend_from_slice(&[0, 0, 0]);
        if version == 1 {
            out.extend_from_slice(&[0; 16]);
            out.extend_from_slice(&timescale.to_be_bytes());
            out.extend_from_slice(&duration.to_be_bytes());
        } else {
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&timescale.to_be_bytes());
            out.extend_from_slice(&(duration as u32).to_be_bytes());
        }
        out
    }

    #[test]
    fn videos_over_desktops_limit_or_not_mp4_are_refused() {
        assert_eq!(mp4_seconds(&mp4(0, 1000, 12_500)), Some(12.5));
        assert_eq!(mp4_seconds(&mp4(1, 600, 600 * 40)), Some(40.));
        assert_eq!(story_file_problem(&mp4(0, 1000, 30_000), "video/mp4"), None);
        assert!(story_file_problem(&mp4(0, 1000, 31_500), "video/mp4").is_some_and(|p| p.contains("too long")));
        assert!(story_file_problem(b"", "video/quicktime").is_some_and(|p| p.contains("unsupported")));
        assert_eq!(story_file_problem(b"", "image/png"), None);
    }
}
