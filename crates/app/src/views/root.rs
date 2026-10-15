//! Top-level window content: link screen or the two-pane chat layout.

use gpui_kit::component::spinner::Spinner;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, Focusable as _, FontWeight, Subscription, Window, div, px};

use crate::state::{AppEvent, AppState, Screen};
use crate::theme::Palette;
use crate::views::conversation::ConversationView;
use crate::views::emoji_picker::EmojiPicker;
use crate::views::image_viewer::{ImageViewer, ViewerEvent};
use crate::views::sidebar::Sidebar;
use crate::{About, FocusSearch, NextConversation, OpenSettings, PreviousConversation, SearchInChat, ShowDebugLog};

const SIDEBAR_WIDTH: f32 = 340.;

pub struct RootView {
    state: Entity<AppState>,
    sidebar: Entity<Sidebar>,
    picker: Entity<EmojiPicker>,
    conversation: Option<Entity<ConversationView>>,
    viewer: Option<(Entity<ImageViewer>, Subscription)>,
    /// The lobby or call, while there is one.
    call: Option<Entity<crate::views::call::CallView>>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(state.clone(), window, cx));
        let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
        let subscriptions = vec![
            // No notifications for the chat you're looking at.
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                this.state.update(cx, |state, _| state.window_active = active);
            }),
            cx.subscribe_in(&state, window, |this, state, event: &AppEvent, window, cx| {
                if let AppEvent::ViewImage(image) = event {
                    let viewer = cx.new(|cx| ImageViewer::new(image.clone(), cx));
                    window.focus(&viewer.focus_handle(cx), cx);
                    let closed = cx.subscribe_in(&viewer, window, |this, _, _: &ViewerEvent, window, cx| {
                        this.viewer = None;
                        // Back to typing where you were.
                        if let Some(conversation) = &this.conversation {
                            window.focus(&conversation.focus_handle(cx), cx);
                        }
                        cx.notify();
                    });
                    this.viewer = Some((viewer, closed));
                    cx.notify();
                }
                if let AppEvent::Error(message) = event {
                    use gpui_kit::component::WindowExt as _;
                    window.push_notification(gpui_kit::component::notification::Notification::error(message.clone()), cx);
                }
                if let AppEvent::SelectionChanged = event {
                    let selected = state.read(cx).selected.clone();
                    this.conversation = selected.map(|id| {
                        let view = cx.new(|cx| ConversationView::new(state.clone(), this.picker.clone(), id, window, cx));
                        let handle = view.read(cx).focus_handle(cx);
                        window.focus(&handle, cx);
                        view
                    });
                    cx.notify();
                }
            }),
            cx.observe(&state, |this, state, cx| {
                let active = {
                    let s = state.read(cx);
                    s.call.is_some() || s.lobby.is_some()
                };
                if active && this.call.is_none() {
                    this.call = Some(cx.new(|cx| crate::views::call::CallView::new(state.clone(), cx)));
                } else if !active {
                    this.call = None;
                }
                cx.notify()
            }),
        ];
        Self {
            state,
            sidebar,
            picker,
            conversation: None,
            viewer: None,
            call: None,
            _subscriptions: subscriptions,
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let screen = self.state.read(cx).screen.clone();
        let syncing = self.state.read(cx).syncing_history().cloned();
        // Calls show through the history transfer too (they ring then).
        let call_shown = matches!(screen, Screen::Main);
        let catching_up = self.state.read(cx).catching_up.clone();
        let body = match screen {
            Screen::Loading => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(palette.background)
                .child(Spinner::new())
                .into_any_element(),
            Screen::Link { .. } => crate::views::link::render(&self.state, window, cx).into_any_element(),
            Screen::Main if let Some(progress) = &syncing => {
                crate::views::syncing::render_history(&self.state, progress, cx).into_any_element()
            }
            Screen::Main if let Some(catch_up) = &catching_up => {
                crate::views::syncing::render_catch_up(catch_up, cx).into_any_element()
            }
            Screen::Main => div()
                .size_full()
                .flex()
                .child(
                    div()
                        .w(px(SIDEBAR_WIDTH))
                        .flex_none()
                        .h_full()
                        .border_r_1()
                        .border_color(palette.panel_border)
                        .child(self.sidebar.clone()),
                )
                .child(div().flex_1().min_w_0().h_full().child(match &self.conversation {
                    Some(view) => view.clone().into_any_element(),
                    None => div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .bg(palette.background)
                        .text_color(palette.text_secondary)
                        .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(palette.text).child("Welcome to Signal"))
                        .child("Select a chat to start messaging")
                        .into_any_element(),
                }))
                .into_any_element(),
        };

        let viewer = self.viewer.as_ref().map(|(viewer, _)| viewer.clone());
        div()
            .key_context("Signal")
            .relative()
            .size_full()
            .font_family(".SystemUIFont")
            .text_color(palette.text)
            .bg(palette.background)
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                if this.state.read(cx).is_loading() {
                    return;
                }
                let search = this.sidebar.read(cx).search_input();
                search.update(cx, |search, cx| search.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &SearchInChat, _, cx| {
                this.state.update(cx, |state, cx| {
                    if let Some(chat) = state.selected.clone() {
                        state.search_in_chat(chat, cx);
                    }
                });
            }))
            .on_action(|_: &About, window, cx| crate::views::about::open(window, cx))
            // Available before linking too, where problems are hardest to see.
            .on_action(cx.listener(|this, _: &ShowDebugLog, window, cx| {
                crate::views::debug_log::open(this.state.clone(), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                let state = this.state.read(cx);
                if matches!(state.screen, Screen::Main) && !state.is_loading() {
                    crate::views::settings::open(this.state.clone(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextConversation, _, cx| {
                this.state.update(cx, |state, cx| {
                    if !state.is_loading() {
                        state.select_relative(1, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &PreviousConversation, _, cx| {
                this.state.update(cx, |state, cx| {
                    if !state.is_loading() {
                        state.select_relative(-1, cx);
                    }
                });
            }))
            .child(body)
            .when_some(self.call.clone().filter(|_| call_shown), |d, call| d.child(call))
            .when_some(viewer, |d, viewer| d.child(viewer))
    }
}
