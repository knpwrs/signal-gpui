//! Full-window video player (Signal Desktop's lightbox for a video): the
//! video fitted to the window, with sound, play and pause (space), a bar to
//! seek with, and the time.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, KeyDownEvent, ObjectFit, RenderImage, Task,
    Window, div, img, px,
};

use crate::state::AppState;
use crate::video_player::VideoPlayer;
use crate::views::image_viewer::ViewerEvent;
use crate::views::lightbox::{self, InChat};

const TOOLBAR_HEIGHT: f32 = 52.;
const CONTROLS_HEIGHT: f32 = 64.;
/// How often a new frame is looked for (a little over 30 fps).
const FRAME_POLL: Duration = Duration::from_millis(30);
/// Steps in the seek bar, each a place to click.
const SEEK_STEPS: usize = 100;

/// A video to play.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewedVideo {
    pub path: PathBuf,
    pub file_name: Option<String>,
    /// A view-once message's copy: deleted when the viewer closes.
    pub view_once: bool,
    /// Its place in its chat, for the lightbox's previous and next,
    /// Forward and "Go to message".
    pub in_chat: Option<InChat>,
}

pub struct VideoViewer {
    video: ViewedVideo,
    state: Option<Entity<AppState>>,
    player: Option<VideoPlayer>,
    error: Option<String>,
    frame: Option<Arc<RenderImage>>,
    focus: FocusHandle,
    _poll: Option<Task<()>>,
}

impl EventEmitter<ViewerEvent> for VideoViewer {}

impl Focusable for VideoViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl VideoViewer {
    /// Opens the video and starts it playing.
    pub fn new(video: ViewedVideo, state: Option<Entity<AppState>>, cx: &mut Context<Self>) -> Self {
        if video.view_once {
            crate::views::image_viewer::delete_on_release(video.path.clone(), cx);
        }
        if let (Some(state), Some(in_chat)) = (&state, video.in_chat.as_ref().filter(|c| c.gallery.is_none())) {
            let this = cx.weak_entity();
            lightbox::load(state, in_chat.conversation.clone(), cx, move |items, cx| {
                let _ = this.update(cx, |this, cx| {
                    if let Some(in_chat) = &mut this.video.in_chat {
                        in_chat.gallery = Some(std::rc::Rc::new(items));
                    }
                    cx.notify();
                });
            });
        }
        let (player, error) = match VideoPlayer::open(&video.path) {
            Ok(player) => {
                player.play();
                (Some(player), None)
            }
            Err(error) => (None, Some(format!("Couldn't play this video: {error:#}"))),
        };
        let poll = player.is_some().then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(FRAME_POLL).await;
                    if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                        break;
                    }
                }
            })
        });
        cx.on_release(|this, cx| {
            if let Some(frame) = this.frame.take() {
                cx.drop_image(frame, None);
            }
        })
        .detach();
        Self { video, state, player, error, frame: None, focus: cx.focus_handle(), _poll: poll }
    }

    /// Shows the newest frame, and keeps the time and controls current.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(player) = &self.player else { return };
        if let Some(frame) = player.frame()
            && let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)
        {
            // (gpui's images are BGRA, as the frames are.)
            let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            if let Some(old) = self.frame.replace(image) {
                cx.drop_image(old, None);
            }
        }
        cx.notify();
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if let Some(player) = &self.player {
            if player.is_playing() {
                player.pause();
            } else {
                // At the end: from the start again.
                if player.duration() > 0. && player.position() >= player.duration() - 0.05 {
                    player.seek(0.);
                }
                player.play();
            }
            cx.notify();
        }
    }

    /// Desktop's previous and next.
    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let (Some(state), Some(in_chat)) = (&self.state, &self.video.in_chat) else { return };
        if let Some(item) = in_chat.step(by) {
            lightbox::show(state, in_chat, &item, cx);
        }
    }

    fn can_step(&self, by: isize) -> bool {
        self.state.is_some() && self.video.in_chat.as_ref().is_some_and(|c| c.step(by).is_some())
    }

    /// Forward, or going to its message: the lightbox closes first.
    fn leave_for(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(state), Some(in_chat)) = (self.state.clone(), self.video.in_chat.clone()) else { return };
        cx.emit(ViewerEvent::Close);
        if forward {
            lightbox::forward(&state, &in_chat, window, cx);
        } else {
            lightbox::go_to(&state, &in_chat, cx);
        }
    }

    /// Desktop's Save: a save dialog.
    fn save(&self, cx: &mut Context<Self>) {
        let name = self.video.file_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| {
            let ext = self.video.path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
            format!("signal-{}.{ext}", chrono::Local::now().format("%Y-%m-%d-%H%M%S"))
        });
        lightbox::save_as(self.video.path.clone(), name, cx);
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Desktop's lightbox: ⌘S saves the one shown (not a view-once copy).
        if event.keystroke.modifiers.platform && event.keystroke.key == "s" {
            if !self.video.view_once {
                self.save(cx);
            }
            return cx.stop_propagation();
        }
        match event.keystroke.key.as_str() {
            "escape" => cx.emit(ViewerEvent::Close),
            "space" => self.toggle(cx),
            "left" => self.step(-1, cx),
            "right" => self.step(1, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl gpui_kit::Render for VideoViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let in_chat = self.video.in_chat.as_ref().filter(|_| self.state.is_some() && !self.video.view_once);
        let tool = |id: &'static str, icon: IconName| {
            div().id(id).debug_selector(move || id.into()).cursor_pointer().child(Icon::new(icon).size_5().text_color(white))
        };
        let (playing, position, duration) = match &self.player {
            Some(p) => (p.is_playing(), p.position(), p.duration()),
            None => (false, 0., 0.),
        };
        let played = if duration > 0. { (position / duration).clamp(0., 1.) } else { 0. };
        let steps = (0..SEEK_STEPS).map(|step| {
            let done = (step as f64 + 0.5) / SEEK_STEPS as f64 <= played;
            div()
                .id(("video-seek", step))
                .flex_1()
                .h(px(16.))
                .flex()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(player) = &this.player {
                        player.seek(step as f64 / SEEK_STEPS as f64 * player.duration());
                        cx.notify();
                    }
                }))
                .child(div().w_full().h(px(4.)).bg(if done { white } else { Hsla { a: 0.3, ..white } }))
        });
        div()
            .id("video-viewer")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .bg(Hsla { h: 0., s: 0., l: 0., a: 0.96 })
            .text_color(white)
            .child(
                div()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    // Clear the macOS traffic lights.
                    .pl(px(92.))
                    .child(lightbox::header(self.video.in_chat.as_ref(), self.video.file_name.as_deref()))
                    .when(in_chat.is_some(), |d| {
                        d.child(tool("forward-video", IconName::Forward).on_click(cx.listener(|this, _, window, cx| this.leave_for(true, window, cx))))
                            .child(
                                tool("go-to-video-message", IconName::MessageSquareShare)
                                    .on_click(cx.listener(|this, _, window, cx| this.leave_for(false, window, cx))),
                            )
                    })
                    .when(!self.video.view_once, |d| {
                        d.child(tool("save-video", IconName::Download).on_click(cx.listener(|this, _, _, cx| this.save(cx))))
                    })
                    .child(tool("video-close", IconName::X).on_click(cx.listener(|_, _, _, cx| cx.emit(ViewerEvent::Close)))),
            )
            .child(
                div()
                    .id("video-frame")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
                    .map(|d| match (&self.frame, &self.error) {
                        (Some(frame), _) => d.child(img(frame.clone()).size_full().object_fit(ObjectFit::Contain)),
                        (None, Some(error)) => d.child(div().text_sm().child(error.clone())),
                        (None, None) => d,
                    }),
            )
            .child(
                div()
                    .h(px(CONTROLS_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_4()
                    .px_6()
                    .child(
                        div()
                            .id("video-play")
                            .size(px(36.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(Hsla { a: 0.15, ..white })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
                            .child(Icon::new(if playing { IconName::Pause } else { IconName::Play }).size_4().text_color(white)),
                    )
                    .child(div().flex_1().flex().items_center().children(steps))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .child(format!("{} / {}", crate::audio::clock(position), crate::audio::clock(duration))),
                    ),
            )
            // Desktop's previous and next, over the sides.
            .children([(-1, "lightbox-previous", IconName::ChevronLeft), (1, "lightbox-next", IconName::ChevronRight)].map(
                |(by, id, icon)| {
                    self.can_step(by).then(|| {
                        div()
                            .absolute()
                            .top(px(TOOLBAR_HEIGHT))
                            .bottom(px(CONTROLS_HEIGHT))
                            .w(px(80.))
                            .when(by < 0, |d| d.left_0())
                            .when(by > 0, |d| d.right_0())
                            .child(lightbox::nav_button(id, icon, by < 0).on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.step(by, cx);
                            })))
                    })
                },
            ).into_iter().flatten())
    }
}
