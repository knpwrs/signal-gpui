//! Desktop's Calls tab (`CallsTab`, `CallsList`, `CallsNewCall`,
//! `CallLinkDetails`): the call history of every chat (each call's notice)
//! and of our call links, searchable and filtered to missed calls; a new
//! call from a picker of chats; "Create a Call Link"; and the details of the
//! chosen row in the pane beside it.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Icon, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, FontWeight, Pixels, Render, SharedString, Subscription, Task, Window,
    div, px,
};
use signal_backend::Command;
use signal_store::{AdhocCall, AdhocCallStatus, CallLink, CallLinkRestrictions, CallOutcome, Conversation, ConversationId, Message, Notice};

use crate::state::{AppEvent, AppState, NavTab};
use crate::theme::{Palette, avatar_colors, color_for};
use crate::views::common::{conversation_avatar, day_label, message_time, short_timestamp};

/// How many calls the list shows.
const LIMIT: u32 = 200;

/// Desktop's default title for a call link without a name.
pub const DEFAULT_LINK_TITLE: &str = "Signal Call";

/// A call link's title.
pub fn link_title(link: Option<&CallLink>) -> String {
    link.map(|l| l.name.trim()).filter(|n| !n.is_empty()).unwrap_or(DEFAULT_LINK_TITLE).to_owned()
}

/// A call link's avatar (Desktop's link icon on the link's color).
pub fn link_avatar(room_id: &str, size: Pixels) -> AnyElement {
    let (bg, fg) = avatar_colors(color_for(&format!("l:{room_id}")));
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size)
        .rounded_full()
        .bg(bg)
        .child(Icon::new(IconName::Link).size(size * 0.45).text_color(fg))
        .into_any_element()
}

/// What the details pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallsSelection {
    Chat(ConversationId),
    /// A call link, by room id.
    Link(String),
}

/// A row of the list.
#[derive(Debug, Clone)]
pub struct CallRow {
    pub key: SharedString,
    pub at: i64,
    pub title: String,
    pub text: String,
    pub missed: bool,
    pub video: bool,
    pub selection: CallsSelection,
    chat: Option<Conversation>,
}

/// A chat's call, in Desktop's call history words.
fn call_words(notice: &Notice, name: impl Fn(&str) -> String) -> Option<(String, bool, bool)> {
    match notice {
        Notice::Call { incoming, video, outcome, .. } => {
            Some((crate::views::notices::call_text(*incoming, *video, *outcome), *outcome == CallOutcome::Missed, *video))
        }
        notice @ Notice::GroupCall { .. } => Some((crate::views::notices::notice_text(notice, name), false, true)),
        _ => None,
    }
}

pub struct CallsPane {
    state: Entity<AppState>,
    calls: Vec<Message>,
    adhoc: Vec<AdhocCall>,
    search: Entity<InputState>,
    /// Desktop's "Filter by missed".
    pub missed_only: bool,
    pub selected: Option<CallsSelection>,
    _load: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CallsPane {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscriptions = vec![
            cx.subscribe(&state, |this, state, event: &AppEvent, cx| match event {
                AppEvent::ConversationsChanged | AppEvent::CallLinksChanged => this.load(cx),
                // A link we made: its details, as Desktop opens them.
                AppEvent::CallLinkCreated(room_id) => {
                    this.selected = Some(CallsSelection::Link(room_id.clone()));
                    state.update(cx, |s, cx| s.set_tab(NavTab::Calls, cx));
                    this.load(cx);
                }
                _ => {}
            }),
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut this =
            Self { state, calls: Vec::new(), adhoc: Vec::new(), search, missed_only: false, selected: None, _load: None, _subscriptions: subscriptions };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let query = backend.spawn(async move { Ok::<_, signal_store::Error>((db.call_history(LIMIT).await?, db.adhoc_call_history(LIMIT).await?)) });
        self._load = Some(cx.spawn(async move |this, cx| {
            if let Ok((calls, adhoc)) = query.await {
                let _ = this.update(cx, |this, cx| {
                    this.calls = calls;
                    this.adhoc = adhoc;
                    cx.notify();
                });
            }
        }));
    }

    #[cfg(test)]
    pub fn search_input(&self) -> Entity<InputState> {
        self.search.clone()
    }

    pub fn set_missed_only(&mut self, on: bool, cx: &mut Context<Self>) {
        self.missed_only = on;
        cx.notify();
    }

    pub fn select(&mut self, selection: Option<CallsSelection>, cx: &mut Context<Self>) {
        self.selected = selection;
        cx.notify();
    }

    /// The rows shown: chats' calls and links' calls (one row per link,
    /// its latest), newest first, filtered and searched as Desktop's list.
    pub fn rows(&self, cx: &App) -> Vec<CallRow> {
        let state = self.state.read(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let name = |sid: &str| state.conversation(&ConversationId::direct(sid)).map(|c| c.title.clone()).unwrap_or_default();
        let mut rows: Vec<CallRow> = self
            .calls
            .iter()
            .filter_map(|call| {
                let chat = state.conversation(&call.conversation_id)?.clone();
                let (text, missed, video) = call_words(call.notice.as_ref()?, name)?;
                Some(CallRow {
                    key: format!("call-{}", call.id).into(),
                    at: call.sent_at,
                    title: chat.title.clone(),
                    text,
                    missed,
                    video,
                    selection: CallsSelection::Chat(chat.id.clone()),
                    chat: Some(chat),
                })
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        for call in &self.adhoc {
            let Some(link) = state.call_link(&call.room_id) else { continue };
            if !seen.insert(call.room_id.clone()) {
                continue;
            }
            rows.push(CallRow {
                key: format!("link-{}", call.room_id).into(),
                at: call.at,
                title: link_title(Some(link)),
                text: "Call link".into(),
                missed: false,
                video: true,
                selection: CallsSelection::Link(call.room_id.clone()),
                chat: None,
            });
        }
        rows.retain(|r| (!self.missed_only || r.missed) && (query.is_empty() || r.title.to_lowercase().contains(&query)));
        rows.sort_by_key(|r| std::cmp::Reverse(r.at));
        rows
    }

    /// A chat's calls, newest first (for its details).
    fn chat_calls(&self, conversation: &ConversationId, cx: &App) -> Vec<(i64, String, bool)> {
        let state = self.state.read(cx);
        let name = |sid: &str| state.conversation(&ConversationId::direct(sid)).map(|c| c.title.clone()).unwrap_or_default();
        self.calls
            .iter()
            .filter(|m| m.conversation_id == *conversation)
            .filter_map(|m| call_words(m.notice.as_ref()?, name).map(|(text, missed, _)| (m.sent_at, text, missed)))
            .collect()
    }

    /// A link's calls, newest first (for its details).
    fn link_calls(&self, room_id: &str) -> Vec<&AdhocCall> {
        self.adhoc.iter().filter(|c| c.room_id == room_id).collect()
    }

    fn empty_text(&self, cx: &App) -> (String, Option<&'static str>) {
        let query = self.search.read(cx).value().trim().to_owned();
        match (query.is_empty(), self.missed_only) {
            (true, false) => ("No calls".into(), Some("Recent calls will appear here.")),
            (true, true) => ("No missed calls".into(), None),
            (false, false) => (format!("No results for “{query}”"), None),
            (false, true) => (format!("No results for “{query}” in missed calls"), None),
        }
    }
}

fn header_button(id: &'static str, icon: IconName, tooltip: &'static str, on: bool, palette: Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .p_1p5()
        .rounded_md()
        .cursor_pointer()
        .when(on, |d| d.bg(palette.row_selected))
        .hover(|d| d.bg(palette.row_hover))
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx))
        .child(Icon::new(icon).size_4().text_color(palette.text))
}

impl Render for CallsPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let collapsed = self.state.read(cx).nav_collapsed;
        let rows = self.rows(cx);
        let (empty_title, empty_subtitle) = self.empty_text(cx);
        let creating = !self.missed_only && self.search.read(cx).value().trim().is_empty();
        let selected = self.selected.clone();
        let state = self.state.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.sidebar)
            .text_color(palette.text)
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pr_3()
                    .when(collapsed, |d| d.mt(crate::views::common::title_bar_inset(window)))
                    .pl(px(16.))
                    .when(collapsed, |d| d.child(crate::views::nav::show_tabs_button(&self.state, palette, cx)))
                    .child(div().flex_1().text_lg().font_weight(FontWeight::SEMIBOLD).child("Calls"))
                    .child(header_button("calls-new-call", IconName::Plus, "New call", false, palette).on_click({
                        let state = state.clone();
                        move |_, window, cx| open_new_call(state.clone(), window, cx)
                    }))
                    .child(header_button("calls-clear", IconName::Trash, "Clear call history", false, palette).on_click({
                        let state = state.clone();
                        move |_, window, cx| confirm_clear(state.clone(), window, cx)
                    }))
                    .child(
                        header_button("calls-missed-filter", IconName::ListFilter, "Filter by missed", self.missed_only, palette)
                            .on_click(cx.listener(|this, _, _, cx| this.set_missed_only(!this.missed_only, cx))),
                    ),
            )
            .child(div().px_3().pb_2().child(Input::new(&self.search)))
            .when(self.missed_only, |d| {
                d.child(div().px_4().pb_1().text_xs().text_color(palette.text_secondary).child("Filtered by missed"))
            })
            .child(
                div()
                    .id("calls-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(creating, |d| {
                        let state = state.clone();
                        d.child(
                            div()
                                .id("calls-create-link")
                                .debug_selector(|| "calls-create-link".into())
                                .mx_2()
                                .px(px(10.))
                                .h(px(56.))
                                .flex()
                                .items_center()
                                .gap_3()
                                .rounded(px(10.))
                                .cursor_pointer()
                                .hover(|d| d.bg(palette.row_hover))
                                .on_click(move |_, _, cx| state.read(cx).send(Command::CreateCallLink))
                                .child(
                                    div()
                                        .size(px(40.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .bg(palette.row_selected)
                                        .child(Icon::new(IconName::Link).size_4().text_color(palette.text)),
                                )
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("Create a Call Link")),
                        )
                    })
                    .when(rows.is_empty(), |d| {
                        d.child(
                            div()
                                .px_4()
                                .py_8()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_1()
                                .text_center()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child(empty_title))
                                .when_some(empty_subtitle, |d, s| d.child(div().text_sm().text_color(palette.text_secondary).child(s))),
                        )
                    })
                    .children(rows.into_iter().map(|row| {
                        let is_selected = selected.as_ref() == Some(&row.selection);
                        let (pick, call_state) = (row.selection.clone(), self.state.clone());
                        let again = row.selection.clone();
                        let video = row.video;
                        let avatar = match (&row.chat, &row.selection) {
                            (Some(chat), _) => conversation_avatar(chat, px(40.)),
                            (None, CallsSelection::Link(room)) => link_avatar(room, px(40.)),
                            (None, CallsSelection::Chat(_)) => crate::views::common::avatar("?", 0, px(40.)).into_any_element(),
                        };
                        let key = row.key.clone();
                        div()
                            .id(row.key.clone())
                            .debug_selector(move || key.to_string())
                            .mx_2()
                            .px(px(10.))
                            .h(px(64.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .rounded(px(10.))
                            .cursor_pointer()
                            .when(is_selected, |d| d.bg(palette.row_selected))
                            .hover(|d| d.bg(palette.row_hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.select(Some(pick.clone()), cx)))
                            .child(avatar)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(row.title.clone()))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .text_color(if row.missed { palette.danger } else { palette.text_secondary })
                                            .child(format!("{} · {}", row.text, short_timestamp(row.at))),
                                    ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("{}-again", row.key)))
                                    .p_2()
                                    .rounded_full()
                                    .hover(|d| d.bg(palette.row_selected))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        let conversation = match &again {
                                            CallsSelection::Chat(id) => id.clone(),
                                            CallsSelection::Link(room) => ConversationId::call_link(room),
                                        };
                                        call_state.update(cx, |s, cx| s.open_call_lobby(conversation, video, cx));
                                    })
                                    .child(Icon::new(if video { IconName::Video } else { IconName::Phone }).size_4().text_color(palette.text)),
                            )
                    })),
            )
    }
}

/// The pane beside the list: the chosen call's details (Desktop's
/// `CallsDetails` / `CallLinkDetails`), or Desktop's placeholder.
pub struct CallDetails {
    state: Entity<AppState>,
    pane: Entity<CallsPane>,
}

impl CallDetails {
    pub fn new(state: Entity<AppState>, pane: Entity<CallsPane>, cx: &mut Context<Self>) -> Self {
        cx.observe(&pane, |_, _, cx| cx.notify()).detach();
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { state, pane }
    }

    fn button(id: &'static str, icon: IconName, label: &'static str, palette: Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
        div()
            .id(id)
            .debug_selector(move || id.into())
            .w(px(84.))
            .py_2()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .rounded_lg()
            .cursor_pointer()
            .bg(palette.row_selected)
            .hover(|d| d.bg(palette.row_hover))
            .child(Icon::new(icon).size_5().text_color(palette.text))
            .child(div().text_xs().child(label))
    }

    fn render_chat(&self, conversation: &ConversationId, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(chat) = self.state.read(cx).conversation(conversation).cloned() else { return placeholder(palette) };
        let calls = self.pane.read(cx).chat_calls(conversation, cx);
        let state = self.state.clone();
        let (message, voice, video) = (state.clone(), state.clone(), state.clone());
        let (id1, id2, id3) = (chat.id.clone(), chat.id.clone(), chat.id.clone());
        let mut list = div().w_full().max_w(px(420.)).flex().flex_col().gap_1();
        let mut last_day = String::new();
        for (at, text, missed) in calls {
            let day = day_label(at);
            if day != last_day {
                list = list.child(div().mt_3().text_sm().font_weight(FontWeight::SEMIBOLD).child(day.clone()));
                last_day = day;
            }
            list = list.child(
                div()
                    .flex()
                    .justify_between()
                    .text_sm()
                    .child(div().text_color(if missed { palette.danger } else { palette.text }).child(text))
                    .child(div().text_color(palette.text_secondary).child(message_time(at))),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .pt(px(64.))
            .px_6()
            .bg(palette.background)
            .text_color(palette.text)
            .child(conversation_avatar(&chat, px(80.)))
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(chat.title.clone()))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(Self::button("call-details-message", IconName::MessageCircle, "Message", palette).on_click(move |_, _, cx| {
                        message.update(cx, |s, cx| {
                            s.set_tab(NavTab::Chats, cx);
                            s.select(Some(id1.clone()), cx);
                        })
                    }))
                    .when(chat.id.as_group().is_none(), |d| {
                        d.child(Self::button("call-details-voice", IconName::Phone, "Voice", palette).on_click(move |_, _, cx| {
                            voice.update(cx, |s, cx| s.open_call_lobby(id2.clone(), false, cx))
                        }))
                    })
                    .child(Self::button("call-details-video", IconName::Video, "Video", palette).on_click(move |_, _, cx| {
                        video.update(cx, |s, cx| s.open_call_lobby(id3.clone(), true, cx))
                    })),
            )
            .child(list)
            .into_any_element()
    }

    fn render_link(&self, room_id: &str, palette: Palette, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(link) = self.state.read(cx).call_link(room_id).cloned() else { return placeholder(palette) };
        let url = signal_backend::call_link_url(&link.root_key).unwrap_or_default();
        let admin = link.is_admin();
        let in_call = {
            let s = self.state.read(cx);
            s.call.as_ref().is_some_and(|c| c.conversation.as_call_link() == Some(room_id))
        };
        let calls = self.pane.read(cx).link_calls(room_id).into_iter().filter(|c| c.status != AdhocCallStatus::Pending).count();
        let conversation = ConversationId::call_link(room_id);
        let state = self.state.clone();
        let approval = link.restrictions == CallLinkRestrictions::AdminApproval;
        let _ = window;
        let row = |id: &'static str, icon: IconName, label: String, danger: bool| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_3()
                .rounded_lg()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .text_color(if danger { palette.danger } else { palette.text })
                .child(Icon::new(icon).size_4())
                .child(label)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .pt(px(64.))
            .px_6()
            .bg(palette.background)
            .text_color(palette.text)
            .child(link_avatar(room_id, px(80.)))
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(link_title(Some(&link))))
            .child(div().text_sm().text_color(palette.text_secondary).child(url.clone()))
            .child(
                Button::new("call-link-join").label("Join").primary().on_click({
                    let state = state.clone();
                    move |_, _, cx| state.update(cx, |s, cx| s.open_call_lobby(conversation.clone(), true, cx))
                }),
            )
            .when(calls > 0, |d| {
                d.child(div().text_sm().text_color(palette.text_secondary).child(match calls {
                    1 => "1 call through this link".to_owned(),
                    n => format!("{n} calls through this link"),
                }))
            })
            .child(
                div()
                    .w_full()
                    .max_w(px(420.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(admin, |d| {
                        let (state, room) = (state.clone(), room_id.to_owned());
                        let label = if link.name.trim().is_empty() { "Add call name" } else { "Edit call name" };
                        let name = link.name.clone();
                        d.child(row("call-link-edit-name", IconName::Pencil, label.into(), false).on_click(move |_, window, cx| {
                            open_edit_name(state.clone(), room.clone(), name.clone(), window, cx)
                        }))
                    })
                    .when(admin, |d| {
                        let (state, room) = (state.clone(), room_id.to_owned());
                        d.child(
                            div()
                                .w_full()
                                .px_3()
                                .py_2()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child("Require admin approval")
                                .child(
                                    Switch::new("call-link-approval").checked(approval).disabled(in_call).on_click(move |on: &bool, _, cx| {
                                        let restrictions = if *on { CallLinkRestrictions::AdminApproval } else { CallLinkRestrictions::None };
                                        state.read(cx).send(Command::UpdateCallLink { room_id: room.clone(), name: None, restrictions: Some(restrictions) });
                                    }),
                                ),
                        )
                    })
                    .child({
                        let url = url.clone();
                        row("call-link-copy", IconName::Copy, "Copy link".into(), false).on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                            window.push_notification(gpui_kit::component::notification::Notification::new().message("Call link copied."), cx);
                        })
                    })
                    .child({
                        let (url, state) = (url.clone(), state.clone());
                        row("call-link-share", IconName::Share2, "Share link via Signal".into(), false)
                            .on_click(move |_, window, cx| open_share_link(state.clone(), url.clone(), window, cx))
                    })
                    .when(!in_call, |d| {
                        let (state, room) = (state.clone(), room_id.to_owned());
                        let label = if admin { "Delete link" } else { "Remove from call history" };
                        d.child(row("call-link-delete", IconName::Trash, label.into(), true).on_click(move |_, window, cx| {
                            confirm_delete(state.clone(), room.clone(), admin, window, cx)
                        }))
                    }),
            )
            .into_any_element()
    }
}

/// Desktop's empty right pane.
fn placeholder(palette: Palette) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(palette.background)
        .text_color(palette.text_secondary)
        .child(Icon::new(IconName::Phone).size_8().text_color(palette.text_secondary))
        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(palette.text).child("Calls"))
        .child("Select a call to see its details")
        .into_any_element()
}

impl Render for CallDetails {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        match self.pane.read(cx).selected.clone() {
            Some(CallsSelection::Chat(id)) => self.render_chat(&id, palette, cx),
            Some(CallsSelection::Link(room)) => self.render_link(&room, palette, window, cx),
            None => placeholder(palette),
        }
    }
}

/// Desktop's "Add call name" / "Edit call name" dialog.
fn open_edit_name(state: Entity<AppState>, room_id: String, name: String, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| {
        let mut input = InputState::new(window, cx).placeholder("Call name");
        input.set_value(name.clone(), window, cx);
        input
    });
    let title = if name.trim().is_empty() { "Add call name" } else { "Edit call name" };
    window.open_dialog(cx, move |dialog, _, _cx| {
        let (state, input_now, room) = (state.clone(), input.clone(), room_id.clone());
        dialog
            .title(title)
            .w(px(400.))
            .child(Input::new(&input))
            .footer(
                DialogFooter::new()
                    .child(Button::new("call-name-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("call-name-save").label("Save").primary().on_click(move |_, window, cx| {
                        let name = input_now.read(cx).value().trim().to_string();
                        state.read(cx).send(Command::UpdateCallLink { room_id: room.clone(), name: Some(name), restrictions: None });
                        window.close_all_dialogs(cx);
                    })),
            )
    });
}

/// Desktop's "Delete call link?", or removing a link we only joined.
fn confirm_delete(state: Entity<AppState>, room_id: String, admin: bool, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let (state, room) = (state.clone(), room_id.clone());
        let (title, body) = if admin {
            ("Delete call link?", "This link will no longer work for anyone who has it.")
        } else {
            ("Remove call link?", "This call link and its calls will be removed from your call history.")
        };
        dialog
            .title(title)
            .w(px(400.))
            .child(div().text_sm().text_color(palette.text_secondary).child(body))
            .footer(
                DialogFooter::new()
                    .child(Button::new("call-link-delete-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("call-link-delete-confirm").label(if admin { "Delete" } else { "Remove" }).danger().on_click(
                        move |_, window, cx| {
                            state.read(cx).send(Command::DeleteCallLink { room_id: room.clone() });
                            window.close_all_dialogs(cx);
                        },
                    )),
            )
    });
}

/// Desktop's "Clear call history?" (`CallsTab`), which also deletes our
/// call links for everyone.
fn confirm_clear(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let links = state.read(cx).call_links.iter().any(CallLink::is_admin);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let state = state.clone();
        let body = if links {
            "This will permanently delete all call history. Call links you’ve created will no longer work for people who have them."
        } else {
            "This will permanently delete all call history."
        };
        dialog
            .title("Clear call history?")
            .w(px(400.))
            .child(div().text_sm().text_color(palette.text_secondary).child(body))
            .footer(
                DialogFooter::new()
                    .child(Button::new("calls-clear-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("calls-clear-confirm").label("Clear").danger().on_click(move |_, window, cx| {
                        state.read(cx).send(Command::ClearCallHistory);
                        window.close_all_dialogs(cx);
                    })),
            )
    });
}

/// Chats a call can go to: not blocked, not left, not the Signal chat or
/// Note to Self.
fn callable(chat: &Conversation) -> bool {
    !chat.blocked && !chat.left && !chat.id.is_signal() && chat.kind != signal_store::ConversationKind::NoteToSelf
}

/// Desktop's new call picker (`CallsNewCall`): chats to call, with a
/// voice button (1:1 only) and a video button.
pub fn open_new_call(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let query = search.read(cx).value().trim().to_lowercase();
        let chats: Vec<Conversation> = state
            .read(cx)
            .conversations
            .iter()
            .filter(|c| callable(c))
            .filter(|c| query.is_empty() || c.title.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let empty = chats.is_empty();
        let rows = chats.into_iter().map(|chat| {
            let (voice, video) = (state.clone(), state.clone());
            let (id1, id2) = (chat.id.clone(), chat.id.clone());
            let call_button = |id: String, icon: IconName| {
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id.clone())
                    .p_2()
                    .rounded_full()
                    .cursor_pointer()
                    .hover(|d| d.bg(palette.row_selected))
                    .child(Icon::new(icon).size_4().text_color(palette.text))
            };
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .hover(|d| d.bg(palette.row_hover))
                .child(conversation_avatar(&chat, px(32.)))
                .child(div().flex_1().truncate().child(chat.title.clone()))
                .when(chat.id.as_group().is_none(), |d| {
                    d.child(call_button(format!("new-call-voice-{}", chat.id.0), IconName::Phone).on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        voice.update(cx, |s, cx| s.open_call_lobby(id1.clone(), false, cx));
                    }))
                })
                .child(call_button(format!("new-call-video-{}", chat.id.0), IconName::Video).on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    video.update(cx, |s, cx| s.open_call_lobby(id2.clone(), true, cx));
                }))
        });
        let empty_text = if query.is_empty() { "No recent conversations.".to_owned() } else { format!("No results for “{query}”") };
        dialog.title("New call").w(px(420.)).child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(Input::new(&search))
                .child(
                    div()
                        .id("new-call-chats")
                        .h(px(360.))
                        .overflow_y_scroll()
                        .when(empty, |d| d.child(div().py_8().text_center().text_color(palette.text_secondary).child(empty_text)))
                        .children(rows),
                ),
        )
    });
}

/// Desktop's "Share link via Signal": the link, sent as a message to the
/// chats chosen.
fn open_share_link(state: Entity<AppState>, url: String, window: &mut Window, cx: &mut App) {
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
    let picked = cx.new(|_| std::collections::BTreeSet::<ConversationId>::new());
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let query = search.read(cx).value().trim().to_lowercase();
        let chosen = picked.read(cx).clone();
        let chats: Vec<Conversation> = state
            .read(cx)
            .conversations
            .iter()
            .filter(|c| !c.blocked && !c.left && !c.id.is_signal())
            .filter(|c| query.is_empty() || c.title.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let rows = chats.into_iter().map(|chat| {
            let on = chosen.contains(&chat.id);
            let (id, picked) = (chat.id.clone(), picked.clone());
            div()
                .id(SharedString::from(format!("share-link-{}", chat.id.0)))
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(move |_, window, cx| {
                    picked.update(cx, |p, _| {
                        if !p.remove(&id) {
                            p.insert(id.clone());
                        }
                    });
                    window.refresh();
                })
                .child(conversation_avatar(&chat, px(32.)))
                .child(div().flex_1().truncate().child(chat.title.clone()))
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .border_2()
                        .border_color(if on { palette.accent } else { palette.text_secondary })
                        .when(on, |d| d.bg(palette.accent)),
                )
        });
        let (state, picked_now, url) = (state.clone(), picked.clone(), url.clone());
        dialog
            .title("Share link via Signal")
            .w(px(420.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&search))
                    .child(div().id("share-link-chats").h(px(320.)).overflow_y_scroll().children(rows)),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("share-link-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("share-link-send").label("Send").primary().disabled(chosen.is_empty()).on_click(
                        move |_, window, cx| {
                            for conversation in picked_now.read(cx).iter().cloned() {
                                state.read(cx).send(Command::SendText {
                                    conversation,
                                    body: url.clone(),
                                    quote: None,
                                    preview: None,
                                    mentions: Vec::new(),
                                    styles: Vec::new(),
                                });
                            }
                            window.close_all_dialogs(cx);
                        },
                    )),
            )
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::{TestAppContext, VisualTestContext, WindowOptions};
    use signal_backend::{Backend, Command, Config};

    use super::*;

    /// A demo backend whose events reach the model, as in `main.rs`.
    fn start(cx: &mut TestAppContext) -> (Backend, Entity<AppState>, tempfile::TempDir) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true, former_data_dir: None };
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
        (backend, state, dir)
    }

    fn wait(cx: &mut TestAppContext, done: &mut dyn FnMut(&mut TestAppContext) -> bool) {
        for _ in 0..600 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn titles(pane: &Entity<CallsPane>, cx: &mut TestAppContext) -> Vec<String> {
        pane.read_with(cx, |pane, cx| pane.rows(cx).into_iter().map(|r| r.title).collect())
    }

    #[gpui_kit::test]
    fn the_list_searches_and_filters_by_missed(cx: &mut TestAppContext) {
        let (backend, state, _dir) = start(cx);
        let (window, pane) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| CallsPane::new(state, window, cx))).unwrap()
        });
        // The demo's calls: Maya's video call, Sam's missed call, our "Book club" link.
        wait(cx, &mut |cx| {
            pane.update(cx, |pane, cx| pane.load(cx));
            cx.run_until_parked();
            titles(&pane, cx).len() == 3
        });
        assert_eq!(titles(&pane, cx), ["Book club", "Maya Chen", "Sam Okafor"], "newest first");
        let missed = pane.read_with(cx, |pane, cx| pane.rows(cx).iter().map(|r| (r.title.clone(), r.missed)).collect::<Vec<_>>());
        assert_eq!(missed[2], ("Sam Okafor".to_owned(), true));

        pane.update(cx, |pane, cx| pane.set_missed_only(true, cx));
        assert_eq!(titles(&pane, cx), ["Sam Okafor"]);
        pane.update(cx, |pane, cx| pane.set_missed_only(false, cx));

        let search = pane.read_with(cx, |pane, _| pane.search_input());
        let mut vcx = VisualTestContext::from_window(window, cx);
        search.update_in(&mut vcx, |input, window, cx| input.set_value("book", window, cx));
        vcx.run_until_parked();
        assert_eq!(titles(&pane, cx), ["Book club"]);
        // Nothing missed matches: Desktop's words.
        pane.update(cx, |pane, cx| pane.set_missed_only(true, cx));
        let empty = pane.read_with(cx, |pane, cx| pane.empty_text(cx).0);
        assert_eq!(empty, "No results for “book” in missed calls");

        // Clearing the history clears everything, our link with it.
        pane.update(cx, |pane, cx| pane.set_missed_only(false, cx));
        let mut vcx = VisualTestContext::from_window(window, cx);
        search.update_in(&mut vcx, |input, window, cx| input.set_value("", window, cx));
        vcx.run_until_parked();
        state.read_with(cx, |s, _| s.send(Command::ClearCallHistory));
        wait(cx, &mut |cx| titles(&pane, cx).is_empty() && state.read_with(cx, |s, _| s.call_links.is_empty()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_new_link_opens_its_details_and_can_be_renamed_and_deleted(cx: &mut TestAppContext) {
        let (backend, state, _dir) = start(cx);
        let (_window, pane) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| CallsPane::new(state, window, cx))).unwrap()
        });
        wait(cx, &mut |cx| state.read_with(cx, |s, _| !s.call_links.is_empty()));
        state.read_with(cx, |s, _| s.send(Command::CreateCallLink));
        wait(cx, &mut |cx| pane.read_with(cx, |p, _| matches!(p.selected, Some(CallsSelection::Link(_)))));
        let Some(CallsSelection::Link(room)) = pane.read_with(cx, |p, _| p.selected.clone()) else { unreachable!() };
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.call_link(&room).is_some()));
        let link = state.read_with(cx, |s, _| s.call_link(&room).cloned().unwrap());
        assert!(link.is_admin());
        assert_eq!(link.restrictions, CallLinkRestrictions::AdminApproval, "Desktop's default");
        assert_eq!(state.read_with(cx, |s, _| s.call_title(&ConversationId::call_link(&room))), DEFAULT_LINK_TITLE);
        // Unused, it's in the history already (Desktop's placeholder).
        wait(cx, &mut |cx| titles(&pane, cx).len() == 4);
        assert_eq!(state.read_with(cx, |s, _| s.tab), NavTab::Calls);

        // Its details, beside the list.
        let details = cx.update(|cx| {
            let (state, pane) = (state.clone(), pane.clone());
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|cx| CallDetails::new(state, pane, cx))).unwrap().0
        });
        let mut vcx = VisualTestContext::from_window(details, cx);
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        for id in ["call-link-edit-name", "call-link-copy", "call-link-share", "call-link-delete"] {
            assert!(vcx.debug_bounds(id).is_some(), "{id}");
        }
        let url = signal_backend::call_link_url(&link.root_key).unwrap();
        assert!(url.starts_with("https://signal.link/call/#key="));

        state.read_with(cx, |s, _| {
            s.send(Command::UpdateCallLink {
                room_id: room.clone(),
                name: Some("  Trail planning  ".into()),
                restrictions: Some(CallLinkRestrictions::None),
            })
        });
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.call_link(&room).is_some_and(|l| l.name == "Trail planning")));
        assert_eq!(state.read_with(cx, |s, _| s.call_link(&room).unwrap().restrictions), CallLinkRestrictions::None);

        state.read_with(cx, |s, _| s.send(Command::DeleteCallLink { room_id: room.clone() }));
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.call_link(&room).is_none()));
        wait(cx, &mut |cx| titles(&pane, cx).len() == 3);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_link_call_lets_people_in_and_goes_in_the_history(cx: &mut TestAppContext) {
        let (backend, state, _dir) = start(cx);
        wait(cx, &mut |cx| state.read_with(cx, |s, _| !s.call_links.is_empty()));
        let room = state.read_with(cx, |s, _| s.call_links[0].room_id.clone());
        let link = ConversationId::call_link(&room);
        state.update(cx, |s, cx| s.open_call_lobby(link.clone(), false, cx));
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.lobby.as_ref().is_some_and(|l| l.peek.is_some())));
        state.update(cx, |s, cx| s.start_call(cx));
        // Leo asks to join the demo's link, ours.
        wait(cx, &mut |cx| {
            state.read_with(cx, |s, _| s.call.as_ref().and_then(|c| c.link.clone()).is_some_and(|l| l.pending.len() == 1))
        });
        let (group, admin, leo) = state.read_with(cx, |s, _| {
            let call = s.call.as_ref().unwrap();
            let link = call.link.as_ref().unwrap();
            (call.group, link.admin, link.pending[0].clone())
        });
        assert!(group && admin);
        state.read_with(cx, |s, _| s.send(Command::ApproveCallJoin { aci: leo.clone(), approve: true }));
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.call.as_ref().is_some_and(|c| c.participants.iter().any(|p| p.aci == leo))));
        assert!(state.read_with(cx, |s, _| s.call.as_ref().unwrap().link.as_ref().unwrap().pending.is_empty()));
        state.update(cx, |s, _| s.hang_up());
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.call.is_none()));
        let db = backend.db().clone();
        let history = futures::executor::block_on(backend.spawn(async move { db.adhoc_call_history(10).await })).unwrap();
        assert_eq!(history.iter().filter(|c| c.room_id == room && c.status == AdhocCallStatus::Joined).count(), 2);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_link_from_a_message_opens_its_lobby(cx: &mut TestAppContext) {
        let (backend, state, _dir) = start(cx);
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.account.is_some()));
        // A made-up V0 key (16 bytes).
        let key: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let url = signal_backend::call_link_url(&key).unwrap();
        state.update(cx, |s, _| s.open_call_link(url));
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.lobby.as_ref().is_some_and(|l| l.conversation.as_call_link().is_some())));
        let room = state.read_with(cx, |s, _| s.lobby.as_ref().unwrap().conversation.as_call_link().unwrap().to_owned());
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.call_link(&room).is_some()));
        assert!(!state.read_with(cx, |s, _| s.call_link(&room).unwrap().is_admin()), "only joined");
        state.update(cx, |s, cx| s.close_call_lobby(cx));
        // A bad link is turned away.
        state.update(cx, |s, _| s.open_call_link("https://signal.link/call/#key=nope".into()));
        wait(cx, &mut |cx| state.read_with(cx, |s, _| s.last_error.as_deref().is_some_and(|e| e.contains("isn't a valid call link"))));
        backend.shutdown();
    }
}
