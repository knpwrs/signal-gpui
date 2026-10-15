//! Desktop's Calls tab, as a list of the call history in every chat (each
//! call's notice): who, which kind ("Missed voice call", "… started a
//! group call"), and when. A row opens its chat; its button calls again.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, FontWeight, Render, SharedString, Task, Window, div, px};
use signal_store::{Message, Notice};

use crate::state::{AppEvent, AppState, NavTab};
use crate::theme::Palette;
use crate::views::common::{conversation_avatar, short_timestamp};

/// How many calls the list shows.
const LIMIT: u32 = 200;

pub struct CallsPane {
    state: Entity<AppState>,
    calls: Vec<Message>,
    _load: Option<Task<()>>,
}

impl CallsPane {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&state, |this, _, event: &AppEvent, cx| {
            if matches!(event, AppEvent::ConversationsChanged) {
                this.load(cx);
            }
        })
        .detach();
        let mut this = Self { state, calls: Vec::new(), _load: None };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let query = backend.spawn(async move { db.call_history(LIMIT).await });
        self._load = Some(cx.spawn(async move |this, cx| {
            if let Ok(calls) = query.await {
                let _ = this.update(cx, |this, cx| {
                    this.calls = calls;
                    cx.notify();
                });
            }
        }));
    }
}

impl Render for CallsPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let collapsed = self.state.read(cx).nav_collapsed;
        let rows: Vec<_> = self
            .calls
            .iter()
            .filter_map(|call| {
                let state = self.state.read(cx);
                let chat = state.conversation(&call.conversation_id)?.clone();
                let (text, missed, video) = match call.notice.as_ref()? {
                    Notice::Call { incoming, video, outcome, .. } => (
                        crate::views::notices::call_text(*incoming, *video, *outcome),
                        *outcome == signal_store::CallOutcome::Missed,
                        *video,
                    ),
                    notice @ Notice::GroupCall { .. } => {
                        (crate::views::notices::notice_text(notice, |sid| state.conversation(&signal_store::ConversationId::direct(sid)).map(|c| c.title.clone()).unwrap_or_default()), false, true)
                    }
                    _ => return None,
                };
                Some((call.id, call.sent_at, chat, text, missed, video))
            })
            .collect();
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
                    .gap_2()
                    .pr_4()
                    .when(collapsed, |d| d.mt(crate::views::common::title_bar_inset(window)))
                    .pl(px(16.))
                    .when(collapsed, |d| d.child(crate::views::nav::show_tabs_button(&self.state, palette, cx)))
                    .child(div().flex_1().text_lg().font_weight(FontWeight::SEMIBOLD).child("Calls")),
            )
            .child(
                div()
                    .id("calls-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(rows.is_empty(), |d| {
                        d.child(div().px_4().py_8().text_center().text_sm().text_color(palette.text_secondary).child("No recent calls"))
                    })
                    .children(rows.into_iter().map(|(id, at, chat, text, missed, video)| {
                        let (open_state, call_state) = (self.state.clone(), self.state.clone());
                        let (open_chat, call_chat) = (chat.id.clone(), chat.id.clone());
                        div()
                            .id(SharedString::from(format!("call-{id}")))
                            .mx_2()
                            .px(px(10.))
                            .h(px(64.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .rounded(px(10.))
                            .cursor_pointer()
                            .hover(|d| d.bg(palette.row_hover))
                            .on_click(move |_, _, cx| {
                                open_state.update(cx, |s, cx| {
                                    s.set_tab(NavTab::Chats, cx);
                                    s.select(Some(open_chat.clone()), cx);
                                })
                            })
                            .child(conversation_avatar(&chat, px(40.)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(chat.title.clone()))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .text_color(if missed { palette.danger } else { palette.text_secondary })
                                            .child(format!("{text} · {}", short_timestamp(at))),
                                    ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("call-again-{id}")))
                                    .p_2()
                                    .rounded_full()
                                    .hover(|d| d.bg(palette.row_selected))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        call_state.update(cx, |s, cx| s.open_call_lobby(call_chat.clone(), video, cx));
                                    })
                                    .child(Icon::new(if video { IconName::Video } else { IconName::Phone }).size_4().text_color(palette.text)),
                            )
                    })),
            )
    }
}
