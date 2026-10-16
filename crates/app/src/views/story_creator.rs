//! Desktop's story creator (`StoryCreator`, `TextStoryCreator`,
//! `SendStoryModal`): a text story with Desktop's styles and backgrounds,
//! or a photo or video with a caption; then where to send it.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyDownEvent, ObjectFit, Render,
    SharedString, Subscription, Window, div, img, px,
};
use signal_backend::{Command, LinkPreviewDraft, OutgoingStory, StoryTarget};
use signal_store::{ConversationId, StoryBackground, StoryFont, StorySendMode, TextStory};

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

/// Desktop's `COLOR_WHITE_INT` and `COLOR_BLACK_INT`.
const WHITE: u32 = 0xffff_ffff;
const BLACK: u32 = 0xff00_0000;

/// How long a link must stay the same before its preview is fetched.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(200);

/// Desktop's text color slider (`ts/mediaEditor/util/color.std.ts`): black
/// up to 7, then the hues from red round to pink, then white from 95.
fn slider_hsl(percentage: f32) -> (f32, f32, f32) {
    const MAX_BLACK: f32 = 7.;
    const MIN_WHITE: f32 = 95.;
    let ratio = |min: f32, max: f32| (percentage - min) / (max - min);
    if percentage <= MAX_BLACK {
        (0., 0.5, 0.5 * ratio(0., MAX_BLACK))
    } else if percentage >= MIN_WHITE {
        (0., 0., (0.5 + ratio(MIN_WHITE, 100.)).min(1.))
    } else {
        (338. * ratio(MAX_BLACK, MIN_WHITE), 1., 0.5)
    }
}

/// The slider's color as ARGB (Desktop's `getRGBANumber`, through its
/// `ts/util/hslToRGB.std.ts`).
pub(crate) fn slider_color(percentage: f32) -> u32 {
    let (h, s, l) = slider_hsl(percentage);
    let a = s * l.min(1. - l);
    let f = |n: f32| {
        let k = (n + h / 30.) % 12.;
        let v = l - a * (k - 3.).min(9. - k).clamp(-1., 1.);
        (255. * v).round().clamp(0., 255.) as u32
    };
    0xff00_0000 | (f(0.) << 16) | (f(8.) << 8) | f(4.)
}

/// The story's text color and its pill's with the slider at `percentage`
/// (Desktop's `TextStoryCreator`): with a pill, white turns to black so
/// the text still shows.
fn text_colors(percentage: f32, background: TextBackground) -> (u32, Option<u32>) {
    let color = slider_color(percentage);
    let light = percentage >= 95.;
    match background {
        TextBackground::None => (color, None),
        TextBackground::Background => (if light { BLACK } else { color }, Some(WHITE)),
        TextBackground::Inverse => (WHITE, Some(if light { BLACK } else { color })),
    }
}

/// An ARGB color, for drawing.
fn argb_hsla(argb: u32) -> Hsla {
    Hsla::from(gpui_kit::Rgba {
        r: ((argb >> 16) & 0xff) as f32 / 255.,
        g: ((argb >> 8) & 0xff) as f32 / 255.,
        b: (argb & 0xff) as f32 / 255.,
        a: (argb >> 24) as f32 / 255.,
    })
}

/// A link whose preview is being looked up, or was.
#[derive(Clone)]
struct FetchedLink {
    url: String,
    /// `None` while it loads; `Some(None)` when there's none.
    preview: Option<Option<Rc<LinkPreviewDraft>>>,
}

/// Desktop's `LinkPreviewApplied`: a preview from a link in the text,
/// which goes when the link does, or one added with the link button,
/// which stays.
#[derive(Clone)]
enum AppliedLink {
    Automatic(Rc<LinkPreviewDraft>),
    Manual(Rc<LinkPreviewDraft>),
}

impl AppliedLink {
    fn preview(&self) -> &Rc<LinkPreviewDraft> {
        match self {
            AppliedLink::Automatic(p) | AppliedLink::Manual(p) => p,
        }
    }
}

/// Desktop's text background: none, a white pill, or the inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextBackground {
    None,
    Background,
    Inverse,
}

/// The boxes in `data`: `(type, contents)`, as MP4 lays them out (a
/// 32-bit size, or 1 and a 64-bit one, or 0 for "to the end").
fn mp4_boxes(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let header = data.get(at..at + 8)?;
        let kind = &header[4..8];
        let size32 = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        let (start, size) = match size32 {
            0 => (at + 8, data.len() - at),
            1 => (at + 16, usize::try_from(u64::from_be_bytes(data.get(at + 8..at + 16)?.try_into().ok()?)).ok()?),
            size => (at + 8, size),
        };
        let end = at.checked_add(size)?;
        if size < start - at || end > data.len() {
            return None;
        }
        let contents = &data[start..end];
        at = end;
        Some((kind, contents))
    })
}

/// An MP4's length in seconds, from its movie header (`moov` → `mvhd`),
/// as Desktop reads it before posting a video.
pub fn mp4_seconds(bytes: &[u8]) -> Option<f64> {
    let (_, moov) = mp4_boxes(bytes).find(|(kind, _)| *kind == b"moov")?;
    let (_, mvhd) = mp4_boxes(moov).find(|(kind, _)| *kind == b"mvhd")?;
    let version = *mvhd.first()?;
    let read = |from: usize, len: usize| -> Option<u64> {
        Some(mvhd.get(from..from + len)?.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    };
    let (timescale, duration) = if version == 1 { (read(20, 4)?, read(24, 8)?) } else { (read(12, 4)?, read(16, 4)?) };
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

/// Desktop's `StoriesSettings__viewers`.
fn viewers(count: usize) -> String {
    if count == 1 { "1 viewer".into() } else { format!("{count} viewers") }
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
    pub(crate) caption: Entity<InputState>,
    /// Desktop's text color slider, 0 to 100 (white at 100).
    pub(crate) color: Entity<SliderState>,
    /// The link button's URL field, and whether it's open.
    pub(crate) link_input: Entity<InputState>,
    pub(crate) link_open: bool,
    /// The last link looked up, from the text or the link field.
    fetched: Option<FetchedLink>,
    /// The link preview the story shows.
    applied: Option<AppliedLink>,
    preview_task: Option<gpui_kit::Task<()>>,
    font: usize,
    text_background: TextBackground,
    background: usize,
    pub(crate) targets: BTreeSet<Target>,
    /// Desktop's "Choose groups" for a new group story: the groups picked
    /// so far, while it's open.
    pub(crate) choosing_groups: Option<BTreeSet<ConversationId>>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    MyStory,
    /// A custom story, by id.
    List(String),
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
        let color = cx.new(|_| SliderState::new().min(0.).max(100.).step(1.).default_value(100.));
        let link_input = cx.new(|cx| InputState::new(window, cx).placeholder("Type or paste a URL"));
        let subscriptions = vec![
            cx.subscribe(&text, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.on_text_changed(&text, cx);
                }
                cx.notify();
            }),
            cx.subscribe(&caption, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.subscribe(&color, |_, _, _: &SliderEvent, cx| cx.notify()),
            cx.subscribe(&link_input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let url = input.read(cx).value().trim().to_owned();
                    if signal_backend::link_preview::should_preview(&url) {
                        this.look_up(url, cx);
                    }
                }
                cx.notify();
            }),
            cx.subscribe(&state, |this, _, event: &crate::state::AppEvent, cx| {
                if let crate::state::AppEvent::LinkPreview { url, preview } = event {
                    this.on_link_preview(url, preview.clone(), cx);
                }
            }),
        ];
        // Desktop's `SendStoryModal` starts with nothing chosen.
        Self {
            state,
            draft,
            choosing: false,
            text,
            caption,
            color,
            link_input,
            link_open: false,
            fetched: None,
            applied: None,
            preview_task: None,
            font: 0,
            text_background: TextBackground::None,
            background: 0,
            targets: BTreeSet::new(),
            choosing_groups: None,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn slider_value(&self, cx: &gpui_kit::App) -> f32 {
        self.color.read(cx).value().start()
    }

    pub(crate) fn text_story(&self, cx: &gpui_kit::App) -> TextStory {
        let (foreground, text_background) = text_colors(self.slider_value(cx), self.text_background);
        TextStory {
            text: self.text.read(cx).value().to_string(),
            font: FONTS[self.font].0,
            foreground: Some(foreground),
            text_background,
            background: backgrounds()[self.background].clone(),
            preview: self.applied.as_ref().map(|applied| {
                let p = applied.preview();
                signal_store::LinkPreview {
                    url: p.url.clone(),
                    title: Some(p.title.clone()).filter(|t| !t.trim().is_empty()),
                    description: p.description.clone(),
                    date: p.date,
                    image: None,
                }
            }),
        }
    }

    /// Desktop's `hasChanges`: some text, or a link preview.
    fn ready(&self, cx: &gpui_kit::App) -> bool {
        match &self.draft {
            Draft::Text => !self.text.read(cx).value().trim().is_empty() || self.applied.is_some(),
            Draft::Media { .. } => true,
        }
    }

    /// Looks up a link's preview once the link has stayed the same a
    /// moment, as the composer does.
    fn look_up(&mut self, url: String, cx: &mut Context<Self>) {
        if let Some(fetched) = self.fetched.as_ref().filter(|f| f.url == url) {
            // Looked up already: a link typed again gets what was found.
            if let Some(preview) = fetched.preview.clone() {
                self.on_link_preview(&url, preview, cx);
            }
            return;
        }
        self.fetched = Some(FetchedLink { url: url.clone(), preview: None });
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.fetched.as_ref().is_some_and(|f| f.url == url && f.preview.is_none()) {
                    this.state.read(cx).send(Command::FetchLinkPreview { url });
                }
            });
        }));
    }

    /// Desktop's text story creator previews the text's first link and
    /// shows it while the link stays in the text, unless one was added
    /// with the link button.
    pub(crate) fn on_text_changed(&mut self, text: &str, cx: &mut Context<Self>) {
        if matches!(self.applied, Some(AppliedLink::Manual(_))) {
            return;
        }
        let links = signal_backend::link_preview::links_in(text);
        if matches!(&self.applied, Some(AppliedLink::Automatic(p)) if !links.contains(&p.url)) {
            self.applied = None;
        }
        if let Some(link) = signal_backend::link_preview::first_previewable(text) {
            self.look_up(link, cx);
        }
    }

    pub(crate) fn on_link_preview(&mut self, url: &str, preview: Option<Rc<LinkPreviewDraft>>, cx: &mut Context<Self>) {
        let Some(fetched) = self.fetched.as_mut().filter(|f| f.url == url) else { return };
        fetched.preview = Some(preview.clone());
        if let Some(preview) = preview
            && !matches!(self.applied, Some(AppliedLink::Manual(_)))
            && signal_backend::link_preview::links_in(self.text.read(cx).value().as_ref()).contains(&preview.url)
        {
            self.applied = Some(AppliedLink::Automatic(preview));
        }
        cx.notify();
    }

    /// The link field's preview, once it's found.
    fn link_field_preview(&self, cx: &gpui_kit::App) -> Option<Rc<LinkPreviewDraft>> {
        let url = self.link_input.read(cx).value().trim().to_owned();
        self.fetched.as_ref().filter(|f| f.url == url).and_then(|f| f.preview.clone().flatten())
    }

    /// The link field's "Add link": the story shows its preview.
    pub(crate) fn add_link(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.link_field_preview(cx) else { return };
        self.applied = Some(AppliedLink::Manual(preview));
        self.link_open = false;
        cx.notify();
    }

    pub(crate) fn send(&mut self, cx: &mut Context<Self>) {
        let story = match &self.draft {
            Draft::Text => OutgoingStory::Text {
                story: Box::new(self.text_story(cx)),
                preview: self.applied.as_ref().map(|a| (**a.preview()).clone()),
            },
            Draft::Media { bytes, content_type, .. } => {
                // Formatting written as in the composer (`*bold*` and so
                // on) styles the caption.
                let (caption, styles) = crate::formatting::parse(self.caption.read(cx).value().trim());
                OutgoingStory::Media { bytes: bytes.clone(), content_type: content_type.clone(), caption, styles }
            }
        };
        let targets: Vec<StoryTarget> = self
            .targets
            .iter()
            .map(|t| match t {
                Target::MyStory => StoryTarget::MyStory,
                Target::List(id) => StoryTarget::List(id.clone()),
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
            if self.choosing_groups.take().is_some() {
                cx.notify();
            } else if self.choosing {
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
                if story.text.trim().is_empty() && story.preview.is_none() {
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
                            })))
                            .child(tagged(
                                "story-link",
                                control("story-link", "Link".into()).on_click(cx.listener(|this, _, window, cx| {
                                    this.link_open = !this.link_open;
                                    if this.link_open {
                                        let input = this.link_input.clone();
                                        input.update(cx, |input, cx| input.focus(window, cx));
                                    }
                                    cx.notify();
                                })),
                            )),
                    )
                    // Desktop's hue slider: the text's color.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().child(Slider::new(&self.color).horizontal()))
                            .child(
                                div()
                                    .size(px(18.))
                                    .flex_none()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(white)
                                    .bg(argb_hsla(slider_color(self.slider_value(cx)))),
                            ),
                    )
                    .when(self.link_open, |d| d.child(self.render_link_field(cx)))
                    .when(self.applied.is_some(), |d| {
                        d.child(
                            Button::new("story-remove-link")
                                .label("Remove link preview")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.applied = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .into_any_element(),
                Draft::Media { .. } => div().w(width).child(Input::new(&self.caption)).into_any_element(),
            })
            .into_any_element()
    }

    /// Picks or unpicks a target. The first time My Story is picked,
    /// Desktop asks who it goes to (`hasFirstStoryPostExperience`); once
    /// that's saved, My Story is picked.
    pub(crate) fn toggle_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.targets.remove(&target) {
            return cx.notify();
        }
        if target == Target::MyStory && !self.state.read(cx).my_story_privacy_set {
            let creator = cx.entity().downgrade();
            let on_saved: crate::views::story_privacy::OnSaved = std::rc::Rc::new(move |_, cx| {
                let _ = creator.update(cx, |c, cx| {
                    c.targets.insert(Target::MyStory);
                    cx.notify();
                });
            });
            crate::views::story_privacy::first_post(self.state.clone(), on_saved, window, cx);
            return;
        }
        self.targets.insert(target);
        cx.notify();
    }

    fn render_targets(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let (groups, lists, my_story, privacy_set) = {
            let state = self.state.read(cx);
            // Desktop's `getGroupStories`: groups whose story is on the list.
            let groups: Vec<(String, String, usize)> =
                state.group_stories().into_iter().filter(|c| !c.blocked).map(|c| (c.id.0.clone(), c.title.clone(), c.members.len())).collect();
            (groups, state.custom_story_lists(), state.my_story(), state.my_story_privacy_set)
        };
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let row = |target: Target, label: String, detail: String, cx: &mut Context<Self>| {
            let on = self.targets.contains(&target);
            let key = match &target {
                Target::MyStory => "story-target-my-story".to_owned(),
                Target::List(id) => format!("story-target-list-{id}"),
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
                .on_click(cx.listener(move |this, _, window, cx| this.toggle_target(target.clone(), window, cx)))
                .child(
                    div()
                        .size(px(18.))
                        .flex_none()
                        .rounded(px(4.))
                        .border_2()
                        .border_color(white)
                        .when(on, |d| d.bg(white)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(label))
                        .child(div().text_xs().opacity(0.7).truncate().child(detail)),
                )
        };
        // Desktop's descriptions (`SendStoryModal`).
        let my_detail = if !privacy_set {
            "Choose who can view your story".to_owned()
        } else if my_story.block_list && !my_story.members.is_empty() {
            format!("All Signal connections · {} excluded", my_story.members.len())
        } else if my_story.members.is_empty() || my_story.block_list {
            "All Signal connections".to_owned()
        } else {
            format!("Only share with · {}", viewers(my_story.members.len()))
        };
        let mut rows = vec![row(Target::MyStory, "My Story".into(), my_detail, cx).into_any_element()];
        for list in lists {
            let detail = format!("Custom story · {}", viewers(list.members.len()));
            rows.push(row(Target::List(list.id), list.name, detail, cx).into_any_element());
        }
        for (id, title, members) in groups {
            let detail = format!("Group story · {}", viewers(members));
            let (state, creator, group) = (self.state.clone(), cx.entity().downgrade(), ConversationId(id.clone()));
            rows.push(
                row(Target::Group(id), title, detail, cx)
                    // Desktop's "Delete story" here takes the group's story
                    // off the list.
                    .context_menu(move |menu, _, _| {
                        let (state, creator, group) = (state.clone(), creator.clone(), group.clone());
                        menu.item(PopupMenuItem::new("Delete story").on_click(move |_, window, cx| {
                            let (state, creator, group) = (state.clone(), creator.clone(), group.clone());
                            window.open_alert_dialog(cx, move |alert, _, _| {
                                let (state, creator, group) = (state.clone(), creator.clone(), group.clone());
                                alert
                                    .description("Remove story? This will remove the story from your list, but you will still be able to view stories from this group.")
                                    .confirm()
                                    .ok_text("Delete")
                                    .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                                    .on_ok(move |_, _, cx| {
                                        let _ = creator.update(cx, |c, cx| {
                                            c.targets.remove(&Target::Group(group.0.clone()));
                                            cx.notify();
                                        });
                                        state.update(cx, |s, cx| s.set_story_send_mode(vec![group.clone()], StorySendMode::Never, cx));
                                        true
                                    })
                            });
                        }))
                    })
                    .into_any_element(),
            );
        }
        let state = self.state.clone();
        rows.push(
            div()
                .pt_2()
                .child(Button::new("story-privacy").label("Story privacy").small().ghost().on_click(move |_, window, cx| {
                    crate::views::story_privacy::open(state.clone(), window, cx);
                }))
                .into_any_element(),
        );
        div()
            .w(px(360.))
            .max_h(px(480.))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().flex_1().text_lg().font_weight(FontWeight::SEMIBOLD).child("Send to"))
                    .child(self.render_new_menu(cx)),
            )
            .child(div().id("story-targets").flex().flex_col().overflow_y_scroll().children(rows))
            .into_any_element()
    }
}

impl StoryCreator {
    /// Desktop's "New" menu on the send screen: a new custom story, or a
    /// new group story.
    fn render_new_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::menu::DropdownMenu as _;
        let (state, creator) = (self.state.clone(), cx.entity().downgrade());
        Button::new("story-new").label("New").small().ghost().dropdown_menu(move |menu, _, _| {
            let (state, creator) = (state.clone(), creator.clone());
            menu.item(PopupMenuItem::new("New custom story").on_click(move |_, window, cx| {
                use crate::views::story_privacy::{Page, Pick, show};
                show(state.clone(), Page::Pick(Pick::NewStory), None, None, window, cx);
            }))
            .item(PopupMenuItem::new("New group story").on_click(move |_, _, cx| {
                let _ = creator.update(cx, |c, cx| {
                    c.choosing_groups = Some(BTreeSet::new());
                    cx.notify();
                });
            }))
        })
    }

    /// Desktop's link field: a URL, its preview, and "Add link".
    fn render_link_field(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let preview = self.link_field_preview(cx);
        let body = match &preview {
            Some(preview) => div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(preview.title.clone()))
                        .child(div().truncate().text_xs().opacity(0.7).child(preview.url.clone())),
                )
                .child(tagged(
                    "story-add-link",
                    Button::new("story-add-link")
                        .label("Add link")
                        .small()
                        .primary()
                        .on_click(cx.listener(|this, _, _, cx| this.add_link(cx))),
                )),
            None => div().text_sm().opacity(0.7).child("Add a link for viewers of your story"),
        };
        div()
            .id("story-link-field")
            .debug_selector(|| "story-link-field".into())
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(8.))
            .bg(Hsla { a: 0.1, ..white })
            .child(Input::new(&self.link_input))
            .child(body)
            .into_any_element()
    }

    /// Desktop's "Choose groups": groups whose story isn't on the list yet.
    fn render_choose_groups(&self, chosen: &BTreeSet<ConversationId>, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let groups = self.state.read(cx).groups_without_stories();
        let rows: Vec<gpui_kit::AnyElement> = groups
            .into_iter()
            .map(|group| {
                let on = chosen.contains(&group.id);
                let key = format!("story-choose-group-{}", group.id.0);
                let id = group.id.clone();
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
                        if let Some(chosen) = &mut this.choosing_groups
                            && !chosen.remove(&id)
                        {
                            chosen.insert(id.clone());
                        }
                        cx.notify();
                    }))
                    .child(div().size(px(18.)).flex_none().rounded(px(4.)).border_2().border_color(white).when(on, |d| d.bg(white)))
                    .child(crate::views::common::conversation_avatar(&group, px(32.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(group.title.clone()))
                            .child(div().text_xs().opacity(0.7).child(members_text(group.members.len()))),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .w(px(360.))
            .max_h(px(480.))
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Choose groups"))
            .when(empty, |d| d.child(div().text_sm().opacity(0.7).child("No groups to add")))
            .child(div().id("story-choose-groups").flex().flex_col().overflow_y_scroll().children(rows))
            .into_any_element()
    }

    /// The groups chosen become group stories (Desktop's
    /// `toggleGroupsForStorySend`), and are picked to send to.
    pub(crate) fn add_chosen_groups(&mut self, cx: &mut Context<Self>) {
        let Some(chosen) = self.choosing_groups.take() else { return };
        if chosen.is_empty() {
            return cx.notify();
        }
        self.targets.extend(chosen.iter().map(|g| Target::Group(g.0.clone())));
        let groups = chosen.into_iter().collect();
        self.state.update(cx, |s, cx| s.set_story_send_mode(groups, StorySendMode::Always, cx));
        cx.notify();
    }
}

/// A button that tests can find.
fn tagged(id: &'static str, button: Button) -> gpui_kit::Div {
    div().debug_selector(move || id.into()).child(button)
}

/// Desktop's `ConversationHero--members`.
fn members_text(count: usize) -> String {
    if count == 1 { "1 member".into() } else { format!("{count} members") }
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
            .child(match (&self.choosing_groups, choosing) {
                (Some(chosen), _) => self.render_choose_groups(&chosen.clone(), cx),
                (None, true) => self.render_targets(cx),
                (None, false) => self.render_editor(height, cx),
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("story-cancel").label("Cancel").ghost().on_click(cx.listener(|this, _, _, cx| {
                        if this.choosing_groups.take().is_some() {
                            cx.notify();
                        } else if this.choosing {
                            this.choosing = false;
                            cx.notify();
                        } else {
                            cx.emit(ViewerEvent::Close);
                        }
                    })))
                    .child(if let Some(chosen) = &self.choosing_groups {
                        tagged(
                            "story-groups-chosen",
                            Button::new("story-groups-chosen")
                                .label("Done")
                                .primary()
                                .disabled(chosen.is_empty())
                                .on_click(cx.listener(|this, _, _, cx| this.add_chosen_groups(cx))),
                        )
                    } else if choosing {
                        tagged(
                            "story-send",
                            Button::new("story-send")
                                .label("Send story")
                                .primary()
                                .disabled(!any_target)
                                .on_click(cx.listener(|this, _, _, cx| this.send(cx))),
                        )
                    } else {
                        tagged(
                            "story-next",
                            Button::new("story-next").label("Next").primary().disabled(!ready).on_click(cx.listener(|this, _, _, cx| {
                                this.choosing = true;
                                cx.notify();
                            })),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], contents: &[u8]) -> Vec<u8> {
        let mut out = ((contents.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(contents);
        out
    }

    fn mp4(version: u8, timescale: u32, duration: u64) -> Vec<u8> {
        let mut mvhd = vec![version, 0, 0, 0];
        if version == 1 {
            mvhd.extend_from_slice(&[0; 16]);
            mvhd.extend_from_slice(&timescale.to_be_bytes());
            mvhd.extend_from_slice(&duration.to_be_bytes());
        } else {
            mvhd.extend_from_slice(&[0; 8]);
            mvhd.extend_from_slice(&timescale.to_be_bytes());
            mvhd.extend_from_slice(&(duration as u32).to_be_bytes());
        }
        let mut out = boxed(b"ftyp", b"isom");
        // Video data before the movie box, holding the bytes "mvhd" (a
        // decoy for anything that searches rather than reads the boxes).
        out.extend(boxed(b"mdat", b"....mvhd\xff\xff\xff\xff...."));
        out.extend(boxed(b"moov", &boxed(b"mvhd", &mvhd)));
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

    #[test]
    fn the_text_color_slider_gives_desktops_colors() {
        // Values worked through Desktop's `getRGBANumber` and `hslToRGB`.
        assert_eq!(slider_color(100.), WHITE);
        assert_eq!(slider_color(0.), BLACK);
        // Halfway along the hues: 169 degrees.
        assert_eq!(slider_color(51.), 0xff00_ffd0);
        // The dark end's last step (half saturated), then the first hue.
        assert_eq!(slider_color(7.), 0xffbf_4040);
        assert_eq!(slider_color(8.), 0xffff_1000);
        // With a pill, white text turns black; an inverse pill takes the color.
        assert_eq!(text_colors(100., TextBackground::None), (WHITE, None));
        assert_eq!(text_colors(100., TextBackground::Background), (BLACK, Some(WHITE)));
        assert_eq!(text_colors(100., TextBackground::Inverse), (WHITE, Some(BLACK)));
        assert_eq!(text_colors(51., TextBackground::Inverse), (WHITE, Some(0xff00_ffd0)));
        assert_eq!(text_colors(51., TextBackground::Background), (0xff00_ffd0, Some(WHITE)));
    }
}
