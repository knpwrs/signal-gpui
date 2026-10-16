//! Calls, laid out as in Signal Desktop: the lobby before a call
//! (`CallingLobby`), the call screen (`CallScreen`), the minimized call
//! (`CallingPip`), and the incoming call bar (`IncomingCallBar`). Call
//! screens are dark whatever the app's theme, as Desktop's are.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, Hsla, ObjectFit, Pixels, RenderImage, Task, Window, div, img,
    px, rgb,
};
use signal_backend::{CallParticipant, CallStatus, CallUpdate, FrameSlot};
use signal_store::{Conversation, ConversationId};

use crate::state::{AppState, CallLobby};
use crate::views::common::{avatar, conversation_avatar};

/// How often a video tile looks for a new frame.
const FRAME_POLL: Duration = Duration::from_millis(33);

/// Desktop's call colors.
fn call_bg() -> Hsla {
    rgb(0x121212).into()
}
fn control_bg() -> Hsla {
    rgb(0x3b3b3b).into()
}
fn end_red() -> Hsla {
    rgb(0xe0352b).into()
}
fn start_green() -> Hsla {
    rgb(0x2a9d4b).into()
}
fn dim_text() -> Hsla {
    rgb(0xb9b9b9).into()
}

/// One video stream (theirs, or our camera), redrawn as frames arrive.
pub struct VideoTile {
    /// Show the whole frame (a shared screen) rather than fill the tile.
    pub fit_whole: bool,
    /// Show a portrait frame (a phone held upright) whole too: filling a
    /// landscape window, it showed only the top of their head.
    pub portrait_whole: bool,
    portrait: bool,
    slot: FrameSlot,
    seq: u64,
    image: Option<Arc<RenderImage>>,
    _poll: Task<()>,
}

impl VideoTile {
    pub fn new(slot: FrameSlot, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME_POLL).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        cx.on_release(|this, cx| {
            if let Some(image) = this.image.take() {
                cx.drop_image(image, None);
            }
        })
        .detach();
        Self { fit_whole: false, portrait_whole: false, portrait: false, slot, seq: 0, image: None, _poll: poll }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(frame) = self.slot.latest() else {
            if let Some(old) = self.image.take() {
                cx.drop_image(old, None);
                cx.notify();
            }
            return;
        };
        if frame.seq == self.seq {
            return;
        }
        self.seq = frame.seq;
        self.portrait = frame.height > frame.width;
        // GPUI's images are BGRA, which is what the slot holds.
        let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra.to_vec()) else { return };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        if let Some(old) = self.image.replace(image) {
            cx.drop_image(old, None);
        }
        cx.notify();
    }

    pub fn has_frame(&self) -> bool {
        self.image.is_some()
    }
}

impl Render for VideoTile {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.image {
            Some(image) => img(image.clone())
                .size_full()
                .object_fit(if self.fit_whole || (self.portrait_whole && self.portrait) {
                    ObjectFit::Contain
                } else {
                    ObjectFit::Cover
                })
                .into_any_element(),
            None => div().size_full().into_any_element(),
        }
    }
}

/// The call (or lobby) on screen; RootView keeps one while either exists.
pub struct CallView {
    state: Entity<AppState>,
    remote: Entity<VideoTile>,
    local: Entity<VideoTile>,
    /// A group call's participants' video, by demux id.
    tiles: HashMap<u32, Entity<VideoTile>>,
    /// Group members' names (by ACI), for the group whose they are.
    names: HashMap<String, String>,
    names_for: Option<ConversationId>,
    /// Ticks the call's clock.
    _clock: Task<()>,
}

impl CallView {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let video = state.read(cx).backend.call_video().clone();
        let remote = cx.new(|cx| VideoTile { portrait_whole: true, ..VideoTile::new(video.remote.clone(), cx) });
        let local = cx.new(|cx| VideoTile::new(video.local.clone(), cx));
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { state, remote, local, tiles: HashMap::new(), names: HashMap::new(), names_for: None, _clock: clock }
    }

    fn chat(&self, id: &ConversationId, cx: &Context<Self>) -> Option<Conversation> {
        self.state.read(cx).conversation(id).cloned()
    }

    fn title(chat: Option<&Conversation>) -> String {
        chat.map_or_else(|| "Unknown".to_owned(), |c| c.title.clone())
    }

    /// A group member's name (once loaded).
    fn name(&self, aci: &str) -> String {
        self.names.get(aci).cloned().unwrap_or_else(|| "Unknown".to_owned())
    }

    /// Loads the group's member names, once per group.
    fn load_names(&mut self, conversation: &ConversationId, cx: &mut Context<Self>) {
        if conversation.as_group().is_none() || self.names_for.as_ref() == Some(conversation) {
            return;
        }
        self.names_for = Some(conversation.clone());
        let Some(members) = self.chat(conversation, cx).map(|c| c.members) else { return };
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let query = backend.spawn(async move { db.display_names(&members).await });
        cx.spawn(async move |this, cx| {
            let names = query.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.names.extend(names);
                cx.notify();
            });
        })
        .detach();
    }

    /// Video tiles for who's in the group call now (and no others).
    fn sync_tiles(&mut self, participants: &[CallParticipant], cx: &mut Context<Self>) {
        let frames = self.state.read(cx).backend.call_video().group.clone();
        self.tiles.retain(|id, _| participants.iter().any(|p| p.demux_id == *id));
        for p in participants {
            self.tiles.entry(p.demux_id).or_insert_with(|| {
                cx.new(|cx| VideoTile { portrait_whole: true, ..VideoTile::new(frames.slot(p.demux_id), cx) })
            });
        }
    }

    fn big_avatar(chat: Option<&Conversation>, size: Pixels) -> AnyElement {
        match chat {
            Some(chat) => conversation_avatar(chat, size),
            None => avatar("?", 0, size).into_any_element(),
        }
    }

    /// Desktop's round call control; `off` shows it inverted (white).
    fn control(
        id: &'static str,
        icon: IconName,
        off: bool,
        tooltip: &'static str,
        on_click: impl Fn(&mut Window, &mut gpui_kit::App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .size(px(56.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .bg(if off { gpui_kit::white() } else { control_bg() })
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx))
            .on_click(move |_, window, cx| on_click(window, cx))
            .child(Icon::new(icon).size_6().text_color(if off { gpui_kit::black() } else { gpui_kit::white() }))
    }

    fn mic_button(&self, muted: bool, on_toggle: impl Fn(&mut gpui_kit::App) + 'static) -> impl IntoElement {
        Self::control(
            "call-mic",
            if muted { IconName::MicOff } else { IconName::Mic },
            muted,
            if muted { "Unmute mic" } else { "Mute mic" },
            move |_, cx| on_toggle(cx),
        )
    }

    /// Desktop's "Present": a picker of screens and windows, or stop.
    fn present_button(&self, sharing: bool) -> impl IntoElement {
        let state = self.state.clone();
        Self::control(
            "call-present",
            if sharing { IconName::ScreenShareOff } else { IconName::ScreenShare },
            sharing,
            if sharing { "Stop presenting" } else { "Present" },
            move |window, cx| {
                if sharing {
                    state.read(cx).send(signal_backend::Command::StopSharing);
                } else {
                    open_share_picker(state.clone(), window, cx);
                }
            },
        )
    }

    fn camera_button(&self, off: bool, on_toggle: impl Fn(&mut gpui_kit::App) + 'static) -> impl IntoElement {
        Self::control(
            "call-camera",
            if off { IconName::VideoOff } else { IconName::Video },
            off,
            if off { "Turn on camera" } else { "Turn off camera" },
            move |_, cx| on_toggle(cx),
        )
    }

    /// Our camera, or "Your camera is off".
    fn self_view(&self, camera_on: bool, cx: &Context<Self>) -> AnyElement {
        if camera_on && self.local.read(cx).has_frame() {
            self.local.clone().into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(control_bg())
                .text_sm()
                .text_color(dim_text())
                .child(if camera_on { "Starting camera…" } else { "Your camera is off" })
                .into_any_element()
        }
    }

    fn render_lobby(&self, lobby: &CallLobby, cx: &mut Context<Self>) -> AnyElement {
        let chat = self.chat(&lobby.conversation, cx);
        let title = Self::title(chat.as_ref());
        let state = self.state.clone();
        let (video, audio) = (lobby.video, lobby.audio);
        let (note, start) = match (&chat, &lobby.peek) {
            (Some(chat), peek) if chat.id.as_group().is_some() => {
                let me = self.state.read(cx).account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
                let mut here: Vec<String> = Vec::new();
                for aci in peek.iter().flatten().filter(|a| **a != me) {
                    if !here.contains(aci) {
                        here.push(aci.clone());
                    }
                }
                let others: Vec<String> = chat.members.iter().filter(|a| **a != me).cloned().collect();
                if !here.is_empty() {
                    let names: Vec<String> = here.iter().map(|a| self.name(a)).collect();
                    (in_this_call(&names), "Join")
                } else if peek.is_none() {
                    (String::new(), "Start")
                } else if others.len() < MAX_RING_SIZE {
                    let names: Vec<String> = others.iter().map(|a| self.name(a)).collect();
                    (format!("Signal will ring {}", name_list(&names)), "Start")
                } else {
                    ("Members of this group will be notified".to_owned(), "Start")
                }
            }
            _ => (format!("Signal will ring {title}"), "Start"),
        };
        div()
            .absolute()
            .inset_0()
            // (Clicks stop here, not on the chat beneath.)
            .occlude()
            .flex()
            .flex_col()
            .bg(call_bg())
            .text_color(gpui_kit::white())
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(52.))
                    .pl(px(92.))
                    .pr_4()
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(title.clone()))
                    .child(
                        div()
                            .id("lobby-close")
                            .cursor_pointer()
                            .on_click({
                                let state = state.clone();
                                move |_, _, cx| state.update(cx, |s, cx| s.close_call_lobby(cx))
                            })
                            .child(Icon::new(IconName::X).size_5().text_color(gpui_kit::white())),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_6()
                    .child(
                        div()
                            .w(px(560.))
                            .h(px(315.))
                            .rounded_xl()
                            .overflow_hidden()
                            .child(if video {
                                self.self_view(true, cx)
                            } else {
                                div()
                                    .size_full()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .justify_center()
                                    .gap_3()
                                    .bg(control_bg())
                                    .child(Self::big_avatar(chat.as_ref(), px(96.)))
                                    .child(div().text_sm().text_color(dim_text()).child("Your camera is off"))
                                    .into_any_element()
                            }),
                    )
                    .child(div().text_sm().text_color(dim_text()).child(note))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .child(self.camera_button(!video, {
                                let state = state.clone();
                                move |cx| state.update(cx, |s, cx| s.set_lobby_video(!video, cx))
                            }))
                            .child(self.mic_button(!audio, {
                                let state = state.clone();
                                move |cx| state.update(cx, |s, cx| s.set_lobby_audio(!audio, cx))
                            }))
                            .child(
                                div()
                                    .id("lobby-start")
                                    .h(px(48.))
                                    .px_8()
                                    .flex()
                                    .items_center()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .bg(start_green())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .on_click(move |_, _, cx| state.update(cx, |s, cx| s.start_call(cx)))
                                    .child(start),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// "Ringing…", "Connecting…", or how long it's been.
    fn status_text(call: &CallUpdate) -> String {
        match &call.status {
            CallStatus::Ringing if call.group => "Incoming group call".into(),
            CallStatus::Ringing => (if call.video { "Incoming video call" } else { "Incoming voice call" }).into(),
            CallStatus::Calling => "Connecting…".into(),
            CallStatus::RingingThem => "Ringing…".into(),
            CallStatus::Connecting => "Connecting…".into(),
            CallStatus::Connected { since } => duration(chrono::Utc::now().timestamp_millis() - since),
        }
    }

    fn render_call(&self, call: &CallUpdate, cx: &mut Context<Self>) -> AnyElement {
        let chat = self.chat(&call.conversation, cx);
        let title = Self::title(chat.as_ref());
        let state = self.state.clone();
        let (audio, video) = {
            let s = self.state.read(cx);
            (s.call_audio, s.call_video)
        };
        let remote_video = (call.remote_video || call.remote_sharing) && self.remote.read(cx).has_frame();
        let connected = matches!(call.status, CallStatus::Connected { .. });
        // A group call shows its header always (its grid has no title).
        let show_header = remote_video || (call.group && !call.participants.is_empty());
        let status = if call.group && connected && call.participants.is_empty() {
            "No one else is here".to_owned()
        } else {
            Self::status_text(call)
        };
        div()
            .absolute()
            .inset_0()
            // (Clicks stop here, not on the chat beneath.)
            .occlude()
            .bg(call_bg())
            .text_color(gpui_kit::white())
            // Their video fills the window; without it, their photo.
            .child(if call.group && !call.participants.is_empty() {
                self.render_grid(call, cx)
            } else if remote_video {
                // A shared screen is shown whole; a face fills the window.
                self.remote.update(cx, |tile, _| tile.fit_whole = call.remote_sharing);
                div().absolute().inset_0().child(self.remote.clone()).into_any_element()
            } else {
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(Self::big_avatar(chat.as_ref(), px(112.)))
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title.clone()))
                    .child(div().text_color(dim_text()).child(status.clone()))
                    .into_any_element()
            })
            .child(
                // Desktop's call header: who, how it's going, and minimize.
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(52.))
                    .pl(px(92.))
                    .pr_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .gap_2()
                            .when(show_header, |d| {
                                d.child(div().font_weight(FontWeight::SEMIBOLD).child(title.clone()))
                                    .child(div().text_color(dim_text()).child(status.clone()))
                            }),
                    )
                    .child(
                        div()
                            .id("call-minimize")
                            .cursor_pointer()
                            .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Minimize call").build(window, cx))
                            .on_click({
                                let state = state.clone();
                                move |_, _, cx| state.update(cx, |s, cx| s.set_call_minimized(true, cx))
                            })
                            .child(Icon::new(IconName::Minimize2).size_5().text_color(gpui_kit::white())),
                    ),
            )
            .child(
                // Our camera, in the corner.
                div()
                    .absolute()
                    .right_4()
                    .bottom(px(104.))
                    .w(px(192.))
                    .h(px(108.))
                    .rounded_lg()
                    .overflow_hidden()
                    .shadow_lg()
                    // (Set apart from a group's tiles beneath it.)
                    .border_2()
                    .border_color(call_bg())
                    .child(self.self_view(video || call.sharing, cx))
                    .when(call.sharing, |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom_1()
                                .left_1()
                                .px_2()
                                .rounded_md()
                                .bg(gpui_kit::black().opacity(0.6))
                                .text_xs()
                                .child("You're presenting"),
                        )
                    }),
            )
            .child(
                div()
                    .absolute()
                    .bottom_6()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .items_center()
                    .gap_4()
                    .child(self.camera_button(!video, {
                        let state = state.clone();
                        move |cx| state.update(cx, |s, cx| s.toggle_call_video(cx))
                    }))
                    .child(self.mic_button(!audio, {
                        let state = state.clone();
                        move |cx| state.update(cx, |s, cx| s.toggle_call_audio(cx))
                    }))
                    .when(connected, |d| d.child(self.present_button(call.sharing)))
                    .child(
                        div()
                            .id("call-end")
                            .h(px(56.))
                            .px_6()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(end_red())
                            .font_weight(FontWeight::SEMIBOLD)
                            .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("End call").build(window, cx))
                            .on_click(move |_, _, cx| state.update(cx, |s, _| s.hang_up()))
                            .child(Icon::new(IconName::PhoneOff).size_5().text_color(gpui_kit::white()))
                            .child(if connected { "End" } else { "Cancel" }),
                    ),
            )
            .into_any_element()
    }

    /// Desktop's group call grid: everyone's video (or photo), named; a
    /// presenter's screen fills the window.
    fn render_grid(&self, call: &CallUpdate, cx: &mut Context<Self>) -> AnyElement {
        if let Some(p) = call.participants.iter().find(|p| p.presenting)
            && let Some(tile) = self.tiles.get(&p.demux_id)
        {
            tile.update(cx, |tile, _| tile.fit_whole = true);
            return div()
                .absolute()
                .inset_0()
                .top(px(52.))
                .bottom(px(96.))
                .child(tile.clone())
                .child(Self::name_tag(format!("{} is presenting", self.name(&p.aci))))
                .into_any_element();
        }
        // Desktop's grid: as square as it can be.
        let count = call.participants.len().max(1);
        let columns = (count as f32).sqrt().ceil() as usize;
        let rows = count.div_ceil(columns);
        let mut grid = div().absolute().left_4().right_4().top(px(52.)).bottom(px(96.)).flex().flex_col().gap_2();
        for row in call.participants.chunks(columns).take(rows) {
            let mut line = div().flex_1().min_h_0().flex().justify_center().gap_2();
            for p in row {
                line = line.child(self.participant_tile(p, cx));
            }
            grid = grid.child(line);
        }
        grid.into_any_element()
    }

    fn participant_tile(&self, p: &CallParticipant, cx: &mut Context<Self>) -> AnyElement {
        let name = self.name(&p.aci);
        let tile = self.tiles.get(&p.demux_id).filter(|t| p.video && t.read(cx).has_frame()).cloned();
        let color = self.chat(&ConversationId::direct(&p.aci), cx).map_or(0, |c| c.color);
        div()
            .relative()
            .flex_1()
            .max_w(px(640.))
            .rounded_lg()
            .overflow_hidden()
            .bg(control_bg())
            .child(match tile {
                Some(tile) => {
                    tile.update(cx, |tile, _| tile.fit_whole = false);
                    div().absolute().inset_0().child(tile).into_any_element()
                }
                None => div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(avatar(&name, color, px(72.)))
                    .into_any_element(),
            })
            .child(Self::name_tag(name))
            .when(!p.audio, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_2()
                        .right_2()
                        .p_1()
                        .rounded_full()
                        .bg(gpui_kit::black().opacity(0.6))
                        .child(Icon::new(IconName::MicOff).size_4().text_color(gpui_kit::white())),
                )
            })
            .into_any_element()
    }

    /// A name over the bottom of a tile.
    fn name_tag(text: String) -> AnyElement {
        div()
            .absolute()
            .bottom_2()
            .left_2()
            .px_2()
            .rounded_md()
            .bg(gpui_kit::black().opacity(0.6))
            .text_sm()
            .child(text)
            .into_any_element()
    }

    /// Desktop's minimized call: a small window in the corner; a click
    /// brings the call back.
    fn render_pip(&self, call: &CallUpdate, cx: &mut Context<Self>) -> AnyElement {
        let chat = self.chat(&call.conversation, cx);
        let state = self.state.clone();
        let group_tile = call
            .participants
            .iter()
            .find(|p| p.presenting)
            .or_else(|| call.participants.iter().find(|p| p.video))
            .and_then(|p| self.tiles.get(&p.demux_id))
            .filter(|t| t.read(cx).has_frame())
            .cloned();
        let remote_video = (call.remote_video || call.remote_sharing) && self.remote.read(cx).has_frame();
        div()
            .id("call-pip")
            .occlude()
            .absolute()
            .right_4()
            .bottom(px(88.))
            .w(px(240.))
            .h(px(135.))
            .rounded_xl()
            .overflow_hidden()
            .shadow_lg()
            .bg(call_bg())
            .text_color(gpui_kit::white())
            .cursor_pointer()
            .on_click({
                let state = state.clone();
                move |_, _, cx| state.update(cx, |s, cx| s.set_call_minimized(false, cx))
            })
            .child(if let Some(tile) = group_tile {
                div().absolute().inset_0().child(tile).into_any_element()
            } else if remote_video {
                div().absolute().inset_0().child(self.remote.clone()).into_any_element()
            } else {
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(Self::big_avatar(chat.as_ref(), px(48.)))
                    .child(div().text_xs().text_color(dim_text()).child(Self::status_text(call)))
                    .into_any_element()
            })
            .child(
                div()
                    .id("pip-end")
                    .absolute()
                    .bottom_2()
                    .right_2()
                    .size(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(end_red())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        state.update(cx, |s, _| s.hang_up());
                    })
                    .child(Icon::new(IconName::PhoneOff).size_4().text_color(gpui_kit::white())),
            )
            .into_any_element()
    }

    /// Desktop's incoming call bar, across the top of the window.
    fn render_incoming(&self, call: &CallUpdate, cx: &mut Context<Self>) -> AnyElement {
        let chat = self.chat(&call.conversation, cx);
        let state = self.state.clone();
        let round = |id: &'static str, icon: IconName, bg: Hsla, tooltip: &'static str| {
            div()
                .id(id)
                .size(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .bg(bg)
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx))
                .child(Icon::new(icon).size_5().text_color(gpui_kit::white()))
        };
        div()
            .absolute()
            .top(px(60.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .w(px(560.))
                    .occlude()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_4()
                    .rounded_xl()
                    .shadow_lg()
                    .bg(rgb(0x2c2c2c))
                    .text_color(gpui_kit::white())
                    .child(Self::big_avatar(chat.as_ref(), px(48.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(Self::title(chat.as_ref())))
                            .child(div().text_sm().text_color(dim_text()).child(match &call.ringer {
                                Some(ringer) => format!("{} is calling you", self.name(ringer)),
                                None => Self::status_text(call),
                            })),
                    )
                    .child(round("call-decline", IconName::PhoneOff, end_red(), "Decline").on_click({
                        let state = state.clone();
                        move |_, _, cx| state.update(cx, |s, _| s.decline_call())
                    }))
                    .when(call.video, |d| {
                        let state = state.clone();
                        d.child(
                            round("call-accept-audio", IconName::VideoOff, control_bg(), "Answer call without video")
                                .on_click(move |_, _, cx| state.update(cx, |s, cx| s.accept_call(false, cx))),
                        )
                    })
                    .child(
                        round(
                            "call-accept",
                            if call.video { IconName::Video } else { IconName::Phone },
                            start_green(),
                            "Answer call",
                        )
                        .on_click({
                            let video = call.video;
                            move |_, _, cx| state.update(cx, |s, cx| s.accept_call(video, cx))
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for CallView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (lobby, call, minimized) = {
            let s = self.state.read(cx);
            (s.lobby.clone(), s.call.clone(), s.call_minimized)
        };
        if let Some(conversation) = call.as_ref().map(|c| &c.conversation).or(lobby.as_ref().map(|l| &l.conversation)) {
            self.load_names(conversation, cx);
        }
        let participants = call.as_ref().map(|c| c.participants.clone()).unwrap_or_default();
        self.sync_tiles(&participants, cx);
        let content = match (&lobby, &call) {
            (_, Some(call)) if call.incoming && call.status == CallStatus::Ringing => self.render_incoming(call, cx),
            (_, Some(call)) if minimized => self.render_pip(call, cx),
            (_, Some(call)) => self.render_call(call, cx),
            (Some(lobby), None) => self.render_lobby(lobby, cx),
            (None, None) => div().into_any_element(),
        };
        // Each positions itself over the window (the root is `relative`):
        // the full screens cover it, the bar and small window don't.
        content
    }
}

/// Desktop's presenting picker: screens, then windows.
fn open_share_picker(state: Entity<AppState>, window: &mut Window, cx: &mut gpui_kit::App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    state.update(cx, |s, _| s.share_sources = None);
    state.read(cx).send(signal_backend::Command::LoadShareSources);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = crate::theme::Palette::get(cx);
        let body = match state.read(cx).share_sources.clone() {
            None => div().text_sm().text_color(palette.text_secondary).child("Looking for screens and windows…").into_any_element(),
            Some(Err(error)) => div()
                .text_sm()
                .text_color(palette.text_secondary)
                .child(format!("Can't list what to share ({error}). Allow screen recording in System Settings → Privacy & Security."))
                .into_any_element(),
            Some(Ok(sources)) => {
                let (screens, windows): (Vec<_>, Vec<_>) = sources.into_iter().partition(|s| s.is_display);
                let group = |title: &'static str, sources: Vec<signal_backend::ShareSource>| {
                    let state = state.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .when(!sources.is_empty(), |d| {
                            d.child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child(title))
                        })
                        .children(sources.into_iter().map(move |source| {
                            let (state, id) = (state.clone(), source.id.clone());
                            Button::new(gpui_kit::SharedString::from(format!("share-{}", source.id)))
                                .ghost()
                                .icon(if source.is_display { IconName::ScreenShare } else { IconName::AppWindow })
                                .label(source.name.clone())
                                .on_click(move |_, window, cx| {
                                    state.read(cx).send(signal_backend::Command::StartSharing { source: id.clone() });
                                    window.close_all_dialogs(cx);
                                })
                        }))
                };
                div()
                    .id("share-sources")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(group("Screens", screens))
                    .child(group("Windows", windows))
                    .into_any_element()
            }
        };
        dialog.title("Present").w(px(440.)).child(body)
    });
}

/// A call's length: "0:42", "12:05", "1:02:03".
/// Desktop rings groups of up to this many members.
const MAX_RING_SIZE: usize = 16;

/// "Ana", "Ana and Leo", "Ana, Leo and 2 others" (Desktop's lobby).
fn name_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} others", rest.len()),
    }
}

/// "Ana is in this call", "Ana and Leo are in this call".
fn in_this_call(names: &[String]) -> String {
    let verb = if names.len() == 1 { "is" } else { "are" };
    format!("{} {verb} in this call", name_list(names))
}

pub fn duration(millis: i64) -> String {
    let total = (millis.max(0) / 1000) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use signal_backend::{Backend, CallStatus, Config};
    use signal_store::{CallOutcome, ConversationId, Notice};

    #[gpui_kit::test]
    fn a_call_goes_from_the_lobby_to_connected_and_into_the_chat(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        // As the app does: backend events into the model.
        cx.update(|cx| {
            use futures::StreamExt as _;
            let state = state.clone();
            cx.spawn(async move |cx| {
                while let Some(event) = events.next().await {
                    state.update(cx, |state, cx| state.handle(event, cx));
                }
            })
            .detach();
        });
        let wait = |cx: &mut TestAppContext, done: &dyn Fn(&crate::state::AppState) -> bool| {
            for _ in 0..600 {
                cx.run_until_parked();
                if state.read_with(cx, |s, _| done(s)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out");
        };
        let maya = ConversationId("d:00000000-0000-4000-8000-00000000a11c".into());
        state.update(cx, |s, cx| s.open_call_lobby(maya.clone(), true, cx));
        assert_eq!(state.read_with(cx, |s, _| s.lobby.as_ref().map(|l| (l.video, l.audio))), Some((true, true)));
        state.update(cx, |s, cx| s.start_call(cx));
        assert!(state.read_with(cx, |s, _| s.lobby.is_none() && s.call_video));
        wait(cx, &|s| matches!(s.call.as_ref().map(|c| &c.status), Some(CallStatus::RingingThem)));
        wait(cx, &|s| matches!(s.call.as_ref().map(|c| &c.status), Some(CallStatus::Connected { .. })));
        // Presenting, from the picker's list, and back.
        state.read_with(cx, |s, _| s.send(signal_backend::Command::LoadShareSources));
        wait(cx, &|s| matches!(&s.share_sources, Some(Ok(sources)) if sources.len() == 2));
        state.read_with(cx, |s, _| s.send(signal_backend::Command::StartSharing { source: "display:1".into() }));
        wait(cx, &|s| s.call.as_ref().is_some_and(|c| c.sharing));
        state.read_with(cx, |s, _| s.send(signal_backend::Command::StopSharing));
        wait(cx, &|s| s.call.as_ref().is_some_and(|c| !c.sharing));
        state.update(cx, |s, _| s.hang_up());
        wait(cx, &|s| s.call.is_none());
        let db = backend.db().clone();
        let id = maya.clone();
        let messages = futures::executor::block_on(backend.spawn(async move { db.messages(&id, None, 50).await })).unwrap();
        let notice = messages.iter().rev().find_map(|m| m.notice.clone());
        assert_eq!(notice, Some(Notice::Call { incoming: false, video: true, outcome: CallOutcome::Accepted, call_id: None, unseen: false }));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn devices_list_and_the_choice_is_kept(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        cx.update(|cx| {
            use futures::StreamExt as _;
            let state = state.clone();
            cx.spawn(async move |cx| {
                while let Some(event) = events.next().await {
                    state.update(cx, |state, cx| state.handle(event, cx));
                }
            })
            .detach();
        });
        let wait = |cx: &mut TestAppContext, done: &dyn Fn(&crate::state::AppState) -> bool| {
            for _ in 0..600 {
                cx.run_until_parked();
                if state.read_with(cx, |s, _| done(s)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out");
        };
        state.read_with(cx, |s, _| s.send(signal_backend::Command::LoadCallDevices));
        wait(cx, &|s| s.call_devices.as_ref().is_some_and(|d| d.devices.microphones.len() == 2));
        assert_eq!(state.read_with(cx, |s, _| s.call_devices.as_ref().unwrap().microphone.clone()), None, "the default");
        state.read_with(cx, |s, _| {
            s.send(signal_backend::Command::SetCallDevice { kind: signal_backend::DeviceKind::Microphone, id: "mic-2".into() })
        });
        wait(cx, &|s| s.call_devices.as_ref().is_some_and(|d| d.microphone.as_deref() == Some("mic-2")));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_group_call_peeks_from_the_lobby_and_shows_everyone(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        cx.update(|cx| {
            use futures::StreamExt as _;
            let state = state.clone();
            cx.spawn(async move |cx| {
                while let Some(event) = events.next().await {
                    state.update(cx, |state, cx| state.handle(event, cx));
                }
            })
            .detach();
        });
        let wait = |cx: &mut TestAppContext, done: &dyn Fn(&crate::state::AppState) -> bool| {
            for _ in 0..600 {
                cx.run_until_parked();
                if state.read_with(cx, |s, _| done(s)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out");
        };
        // The demo's hiking group, whose call Maya's in.
        let group = ConversationId("g:0000000000000000000000000000000000000000000000000000000000000001".into());
        state.update(cx, |s, cx| s.open_call_lobby(group.clone(), true, cx));
        wait(cx, &|s| s.lobby.as_ref().is_some_and(|l| l.peek.is_some()));
        let peek = state.read_with(cx, |s, _| s.lobby.as_ref().unwrap().peek.clone().unwrap());
        assert_eq!(peek, ["00000000-0000-4000-8000-00000000a11c"]);
        state.update(cx, |s, cx| s.start_call(cx));
        wait(cx, &|s| matches!(s.call.as_ref().map(|c| &c.status), Some(CallStatus::Connected { .. })));
        let (group_call, people) = state.read_with(cx, |s, _| {
            let call = s.call.as_ref().unwrap();
            (call.group, call.participants.len())
        });
        assert!(group_call);
        assert_eq!(people, 4, "the group's members");
        state.update(cx, |s, _| s.hang_up());
        wait(cx, &|s| s.call.is_none());
        let db = backend.db().clone();
        let id = group.clone();
        let messages = futures::executor::block_on(backend.spawn(async move { db.messages(&id, None, 50).await })).unwrap();
        assert!(messages.iter().any(|m| matches!(m.notice, Some(Notice::GroupCall { by: None, incoming: false, status: Some(signal_store::GroupCallStatus::Generic), .. }))));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_camera_that_fails_turns_off_and_says_why(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, _events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let maya = ConversationId("d:00000000-0000-4000-8000-00000000a11c".into());
        state.update(cx, |s, cx| s.open_call_lobby(maya, true, cx));
        state.update(cx, |s, cx| s.handle(signal_backend::Event::CameraFailed("no camera".into()), cx));
        let (video, error) = state.read_with(cx, |s, _| (s.lobby.as_ref().unwrap().video, s.last_error.clone()));
        assert!(!video, "the toggle shows it off");
        assert_eq!(error.as_deref(), Some("Couldn't start your camera: no camera"));
        backend.shutdown();
    }

    #[test]
    fn lobby_names_read_like_desktop() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::in_this_call(&names(&["Ana"])), "Ana is in this call");
        assert_eq!(super::in_this_call(&names(&["Ana", "Leo"])), "Ana and Leo are in this call");
        assert_eq!(super::name_list(&names(&["Ana", "Leo", "Maya"])), "Ana, Leo and Maya");
        assert_eq!(super::name_list(&names(&["Ana", "Leo", "Maya", "Sam"])), "Ana, Leo and 2 others");
    }

    #[test]
    fn durations_read_like_desktop() {
        assert_eq!(super::duration(42_000), "0:42");
        assert_eq!(super::duration(725_000), "12:05");
        assert_eq!(super::duration(3_723_000), "1:02:03");
        assert_eq!(super::duration(-5), "0:00");
    }
}
