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

/// Settings key: the chat list's width as last dragged (not a Signal
/// setting; Desktop keeps its own the same way).
const SETTING_SIDEBAR_WIDTH: &str = "sidebar_width";

/// What's dragged to resize the chat list.
struct SidebarResize;

pub struct RootView {
    state: Entity<AppState>,
    sidebar: Entity<Sidebar>,
    picker: Entity<EmojiPicker>,
    conversation: Option<Entity<ConversationView>>,
    viewer: Option<(Entity<ImageViewer>, Subscription)>,
    video_viewer: Option<(Entity<crate::views::video_viewer::VideoViewer>, Subscription)>,
    /// The lobby or call, while there is one.
    call: Option<Entity<crate::views::call::CallView>>,
    /// The chat list's width as dragged (see `crate::sidebar_width`).
    sidebar_preferred: f32,
    /// Whether an account was ready when last seen.
    had_account: bool,
    /// Counts account changes, so a width read for an earlier account
    /// isn't applied to the next.
    account_generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(state.clone(), window, cx));
        let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
        let subscriptions = vec![
            cx.subscribe(&sidebar, |this, _, event: &crate::views::sidebar::SidebarEvent, cx| match event {
                crate::views::sidebar::SidebarEvent::Expand => this.expand_sidebar(cx),
            }),
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
                if let AppEvent::ViewVideo(video) = event {
                    let viewer = cx.new(|cx| crate::views::video_viewer::VideoViewer::new(video.clone(), cx));
                    window.focus(&viewer.focus_handle(cx), cx);
                    let closed = cx.subscribe_in(&viewer, window, |this, _, _: &ViewerEvent, window, cx| {
                        // Dropping it stops the video.
                        this.video_viewer = None;
                        if let Some(conversation) = &this.conversation {
                            window.focus(&conversation.focus_handle(cx), cx);
                        }
                        cx.notify();
                    });
                    this.video_viewer = Some((viewer, closed));
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
                // The width is the account's setting: an unlink wipes it, so
                // start over, and read the next account's once it's ready.
                let has_account = state.read(cx).account.is_some();
                if this.had_account != has_account {
                    this.account_generation += 1;
                }
                if this.had_account && !has_account {
                    this.sidebar_preferred = crate::sidebar_width::DEFAULT_WIDTH;
                } else if !this.had_account && has_account {
                    this.load_sidebar_width(cx);
                }
                this.had_account = has_account;
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
        let this = Self {
            state,
            sidebar,
            picker,
            conversation: None,
            viewer: None,
            video_viewer: None,
            call: None,
            sidebar_preferred: crate::sidebar_width::DEFAULT_WIDTH,
            had_account: false,
            account_generation: 0,
            _subscriptions: subscriptions,
        };
        // (The width loads once the account is ready: see the observer.)
        this
    }

    /// The width last dragged to, from the settings.
    fn load_sidebar_width(&self, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let generation = self.account_generation;
        cx.spawn(async move |this, cx| {
            let db = backend.db().clone();
            let saved = backend.spawn(async move { db.setting(SETTING_SIDEBAR_WIDTH).await }).await;
            let width = saved.ok().flatten().and_then(|w| w.parse::<f32>().ok());
            if let Some(width) = width.filter(|w| w.is_finite()) {
                let _ = this.update(cx, |this, cx| {
                    if this.account_generation == generation {
                        this.sidebar_preferred = width;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Settles the width (snapped, as drawn) and keeps it.
    fn save_sidebar_width(&mut self, cx: &mut Context<Self>) {
        let requires_full = self.sidebar.read(cx).requires_full_width();
        // A forced full width isn't a choice: keep the narrow one chosen.
        if !requires_full {
            self.sidebar_preferred = crate::sidebar_width::width(self.sidebar_preferred, false);
        }
        let width = self.sidebar_preferred;
        let backend = self.state.read(cx).backend.clone();
        cx.spawn(async move |_, _| {
            let db = backend.db().clone();
            let _ = backend.spawn(async move { db.set_setting(SETTING_SIDEBAR_WIDTH, &width.to_string()).await }).await;
        })
        .detach();
        cx.notify();
    }

    /// Desktop's `expandNarrowLeftPane`.
    fn expand_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_preferred < crate::sidebar_width::MIN_FULL_WIDTH {
            self.sidebar_preferred = crate::sidebar_width::MIN_FULL_WIDTH;
            self.save_sidebar_width(cx);
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let screen = self.state.read(cx).screen.clone();
        // Desktop's left pane width: narrow (avatars) below the snap.
        let requires_full = self.sidebar.read(cx).requires_full_width();
        let sidebar_width = crate::sidebar_width::width(self.sidebar_preferred, requires_full);
        let narrow = crate::sidebar_width::is_narrow(sidebar_width);
        if self.sidebar.read(cx).narrow != narrow {
            self.sidebar.update(cx, |sidebar, cx| {
                sidebar.narrow = narrow;
                cx.notify();
            });
        }
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
                // Dragging the chat list's edge (Desktop's drag handle).
                .on_drag_move(cx.listener(|this, event: &gpui_kit::DragMoveEvent<SidebarResize>, _, cx| {
                    this.sidebar_preferred = f32::from(event.event.position.x);
                    cx.notify();
                }))
                .on_drop(cx.listener(|this, _: &SidebarResize, _, cx| this.save_sidebar_width(cx)))
                .child(
                    div()
                        .relative()
                        .w(px(sidebar_width))
                        .flex_none()
                        .h_full()
                        .border_r_1()
                        .border_color(palette.panel_border)
                        .child(self.sidebar.clone())
                        .child(
                            div()
                                .id("sidebar-resize")
                                .debug_selector(|| "sidebar-resize".into())
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right(px(-4.))
                                .w(px(8.))
                                .cursor_col_resize()
                                .on_drag(SidebarResize, |_, _, _, cx| cx.new(|_| gpui_kit::EmptyView)),
                        ),
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
        let video_viewer = self.video_viewer.as_ref().map(|(viewer, _)| viewer.clone());
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
                // A narrow list widens to search, as Desktop's does.
                this.expand_sidebar(cx);
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
            .when_some(video_viewer, |d, viewer| d.child(viewer))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Modifiers, MouseButton, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, point, size};
    use signal_backend::{Backend, Config};

    use super::*;

    #[gpui_kit::test]
    fn dragging_the_list_narrow_shows_avatars_and_search_widens_it(cx: &mut TestAppContext) {
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
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(1000.), px(700.)) })),
            ..Default::default()
        };
        let (window, root) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| RootView::new(state, window, cx))).unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        let mut handle = None;
        for _ in 0..300 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            handle = cx.debug_bounds("sidebar-resize");
            if handle.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let handle = handle.expect("the chat list's edge").center();
        assert!(!root.read_with(&cx, |r, cx| r.sidebar.read(cx).narrow));

        // Drag the edge to 150: under the snap, so narrow.
        cx.simulate_mouse_down(handle, MouseButton::Left, Modifiers::none());
        for x in [300., 250., 200., 150.] {
            cx.simulate_mouse_move(point(px(x), handle.y), MouseButton::Left, Modifiers::none());
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(point(px(150.), handle.y), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        root.read_with(&cx, |r, cx| {
            assert!(r.sidebar.read(cx).narrow, "avatars only");
            assert_eq!(r.sidebar_preferred, crate::sidebar_width::MIN_WIDTH, "snapped and kept");
        });

        // Its search button (or a megaphone) widens it.
        root.update(&mut cx, |r, cx| r.expand_sidebar(cx));
        cx.run_until_parked();
        root.read_with(&cx, |r, cx| {
            assert!(!r.sidebar.read(cx).narrow);
            assert_eq!(r.sidebar_preferred, crate::sidebar_width::MIN_FULL_WIDTH);
        });
        backend.shutdown();
    }
}
