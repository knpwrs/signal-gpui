//! Top-level window content: link screen or the two-pane chat layout.

use gpui_kit::component::spinner::Spinner;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, Focusable as _, FontWeight, Subscription, Window, div, px};

use crate::state::{AppEvent, AppState, NavTab, Screen};
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
    /// The Stories and Calls tabs' lists.
    stories: Entity<crate::views::stories::StoriesPane>,
    calls: Entity<crate::views::calls::CallsPane>,
    story_viewer: Option<(Entity<crate::views::story_viewer::StoryViewer>, Subscription)>,
    story_creator: Option<(Entity<crate::views::story_creator::StoryCreator>, Subscription)>,
    /// The chat list's width as dragged (see `crate::sidebar_width`).
    sidebar_preferred: f32,
    /// The account (its ACI) when last seen.
    account: Option<String>,
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
                // Desktop refreshes its remote config when it becomes active.
                if active {
                    this.state.read(cx).send(signal_backend::Command::RefreshRemoteConfig);
                }
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
                if let AppEvent::ViewStories(chat) = event {
                    let viewer = cx.new(|cx| crate::views::story_viewer::StoryViewer::new(state.clone(), chat.clone(), window, cx));
                    window.focus(&viewer.focus_handle(cx), cx);
                    let closed = cx.subscribe_in(&viewer, window, |this, _, _: &ViewerEvent, _, cx| {
                        // Dropping it stops a video.
                        this.story_viewer = None;
                        cx.notify();
                    });
                    this.story_viewer = Some((viewer, closed));
                    cx.notify();
                }
                if let AppEvent::CreateStory(media) = event {
                    let creator = cx.new(|cx| match media.clone() {
                        None => crate::views::story_creator::StoryCreator::text(state.clone(), window, cx),
                        Some((bytes, content_type, path)) => {
                            crate::views::story_creator::StoryCreator::media(state.clone(), bytes, content_type, path, window, cx)
                        }
                    });
                    window.focus(&creator.focus_handle(cx), cx);
                    let closed = cx.subscribe_in(&creator, window, |this, _, _: &ViewerEvent, _, cx| {
                        this.story_creator = None;
                        cx.notify();
                    });
                    this.story_creator = Some((creator, closed));
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
                this.follow_account(cx);
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
        let stories = cx.new(|cx| crate::views::stories::StoriesPane::new(state.clone(), cx));
        let calls = cx.new(|cx| crate::views::calls::CallsPane::new(state.clone(), cx));
        let mut this = Self {
            state,
            sidebar,
            picker,
            conversation: None,
            viewer: None,
            video_viewer: None,
            call: None,
            stories,
            calls,
            story_viewer: None,
            story_creator: None,
            sidebar_preferred: crate::sidebar_width::DEFAULT_WIDTH,
            account: None,
            account_generation: 0,
            _subscriptions: subscriptions,
        };
        this.follow_account(cx);
        this
    }

    /// The width is the account's setting (an unlink wipes it): with each
    /// account, start over and read its own.
    fn follow_account(&mut self, cx: &mut Context<Self>) {
        let account = self.state.read(cx).account.as_ref().map(|a| a.aci.clone());
        if account == self.account {
            return;
        }
        self.account = account;
        self.account_generation += 1;
        self.sidebar_preferred = crate::sidebar_width::DEFAULT_WIDTH;
        if self.account.is_some() {
            self.load_sidebar_width(cx);
        }
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
        let (tab, nav_collapsed) = {
            let s = self.state.read(cx);
            (s.tab, s.nav_collapsed)
        };
        let rail = if nav_collapsed { 0. } else { crate::views::nav::NAV_WIDTH };
        // Desktop's left pane width: narrow (avatars) below the snap; the
        // Stories and Calls lists always need their names.
        let requires_full = self.sidebar.read(cx).requires_full_width() || tab != NavTab::Chats;
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
                // Desktop's tabs, unless hidden.
                .when(!nav_collapsed, |d| d.child(crate::views::nav::render(&self.state, palette, window, cx)))
                // Dragging the chat list's edge (Desktop's drag handle).
                .on_drag_move(cx.listener(move |this, event: &gpui_kit::DragMoveEvent<SidebarResize>, _, cx| {
                    this.sidebar_preferred = f32::from(event.event.position.x) - rail;
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
                        .child(match tab {
                            NavTab::Chats => self.sidebar.clone().into_any_element(),
                            NavTab::Stories => self.stories.clone().into_any_element(),
                            NavTab::Calls => self.calls.clone().into_any_element(),
                        })
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
                .child(div().flex_1().min_w_0().h_full().child(match (tab, &self.conversation) {
                    (NavTab::Stories, _) => crate::views::stories::render_placeholder(palette).into_any_element(),
                    (_, Some(view)) if tab == NavTab::Chats => view.clone().into_any_element(),
                    (_, _) => div()
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
        let story_viewer = self.story_viewer.as_ref().map(|(viewer, _)| viewer.clone());
        let story_creator = self.story_creator.as_ref().map(|(creator, _)| creator.clone());
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
            .when_some(story_viewer, |d, viewer| d.child(viewer))
            .when_some(story_creator, |d, creator| d.child(creator))
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

    #[gpui_kit::test]
    fn the_stories_tab_lists_stories_and_opens_them(cx: &mut TestAppContext) {
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
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(1000.), px(900.)) })),
            ..Default::default()
        };
        let (window, root) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| RootView::new(state, window, cx))).unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        let wait = |cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            for _ in 0..300 {
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out waiting for {what}");
        };
        wait(&mut cx, "the tabs and the demo's stories", &|cx| {
            cx.debug_bounds("nav-stories").is_some() && state.read_with(cx, |s, _| s.story_groups().len() >= 3)
        });
        // Its badge counts the people and groups with something new.
        assert_eq!(state.read_with(&cx, |s, _| s.unread_story_chats()), 3);
        let tab = cx.debug_bounds("nav-stories").unwrap().center();
        cx.simulate_click(tab, Modifiers::none());
        wait(&mut cx, "the stories list", &|cx| cx.debug_bounds("my-stories").is_some());

        // The first row opens the viewer, which marks the story viewed.
        let first = state.read_with(&cx, |s, _| s.story_groups()[0].chat.clone());
        let row = cx.debug_bounds(Box::leak(format!("story-row-{}", first.0).into_boxed_str())).expect("its row").center();
        cx.simulate_click(row, Modifiers::none());
        wait(&mut cx, "the viewer", &|cx| cx.debug_bounds("story-viewer").is_some());
        wait(&mut cx, "the story to be viewed", &|cx| {
            state.read_with(cx, |s, _| s.story_groups().iter().any(|g| g.chat == first && !g.unread()))
        });
        cx.update(|window, cx| window.dispatch_keystroke(gpui_kit::Keystroke::parse("escape").unwrap(), cx));
        wait(&mut cx, "the viewer to close", &|cx| cx.debug_bounds("story-viewer").is_none());
        let _ = root;
        backend.shutdown();
    }

    /// The demo in a window, with backend events pumped into the model as
    /// `main.rs` does.
    fn demo_window(
        cx: &mut TestAppContext,
    ) -> (VisualTestContext, Entity<AppState>, Entity<RootView>, Backend, tempfile::TempDir) {
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
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(1000.), px(900.)) })),
            ..Default::default()
        };
        let (window, root) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| RootView::new(state, window, cx))).unwrap()
        });
        (VisualTestContext::from_window(window, cx), state, root, backend, dir)
    }

    fn wait(cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..300 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn click(cx: &mut VisualTestContext, what: &'static str) {
        wait(cx, what, &|cx| cx.debug_bounds(what).is_some());
        let at = cx.debug_bounds(what).unwrap().center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
    }

    /// The demo's Jordan Reyes.
    const JORDAN: &str = "00000000-0000-4000-8000-0000000000b0";
    /// The demo's Maya Chen.
    const MAYA: &str = "00000000-0000-4000-8000-00000000a11c";

    #[gpui_kit::test]
    fn a_text_story_is_written_and_posted(cx: &mut TestAppContext) {
        let (mut cx, state, root, backend, _dir) = demo_window(cx);
        wait(&mut cx, "the demo", &|cx| state.read_with(cx, |s, _| !s.my_stories().is_empty()));
        let before = state.read_with(&cx, |s, _| s.my_stories().len());
        state.update(&mut cx, |s, cx| s.create_text_story(cx));
        wait(&mut cx, "the creator", &|cx| cx.debug_bounds("story-creator").is_some());
        let creator = root.read_with(&cx, |r, _| r.story_creator.as_ref().map(|(c, _)| c.clone())).unwrap();
        cx.update(|window, cx| {
            creator.update(cx, |c, cx| {
                c.text.update(cx, |input, cx| input.set_value("Out on the trail", window, cx));
                c.choosing = true;
                cx.notify();
            })
        });
        // Nothing is chosen to start with, as in Desktop.
        wait(&mut cx, "the targets", &|cx| cx.debug_bounds("story-target-my-story").is_some());
        assert!(creator.read_with(&cx, |c, _| c.targets.is_empty()));

        // The first time, My Story asks who it goes to: all but Jordan.
        click(&mut cx, "story-target-my-story");
        wait(&mut cx, "My Story privacy", &|cx| cx.debug_bounds("story-privacy").is_some());
        assert!(creator.read_with(&cx, |c, _| c.targets.is_empty()), "not until it's saved");
        click(&mut cx, "privacy-except");
        click(&mut cx, Box::leak(format!("privacy-pick-{JORDAN}").into_boxed_str()));
        click(&mut cx, "privacy-picked");
        click(&mut cx, "privacy-save");
        wait(&mut cx, "My Story chosen", &|cx| creator.read_with(cx, |c, _| c.targets.contains(&crate::views::story_creator::Target::MyStory)));
        state.read_with(&cx, |s, _| {
            assert!(s.my_story_privacy_set);
            let my = s.my_story();
            assert!(my.block_list && my.members == [JORDAN]);
        });

        creator.update(&mut cx, |c, cx| c.send(cx));
        wait(&mut cx, "the story to post", &|cx| state.read_with(cx, |s, _| s.my_stories().len() == before + 1));
        let posted = state.read_with(&cx, |s, _| s.my_stories().last().cloned().unwrap());
        assert!(matches!(posted.content, signal_store::StoryContent::Text(ref t) if t.text == "Out on the trail"));
        let to: Vec<&str> = posted.recipients.iter().map(|r| r.service_id.as_str()).collect();
        assert!(to.contains(&MAYA) && !to.contains(&JORDAN), "{to:?}");
        assert!(cx.debug_bounds("story-creator").is_none(), "closed once sent");
        // Saved for next time: the backend kept it.
        let saved = backend.spawn({
            let db = backend.db().clone();
            async move { db.setting(signal_backend::SETTING_MY_STORY_PRIVACY_SET).await }
        });
        assert_eq!(futures::executor::block_on(saved).unwrap().as_deref(), Some("true"));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_custom_story_is_made_posted_to_and_deleted(cx: &mut TestAppContext) {
        let (mut cx, state, root, backend, _dir) = demo_window(cx);
        wait(&mut cx, "the demo", &|cx| state.read_with(cx, |s, _| !s.my_stories().is_empty()));
        let privacy = cx
            .update(|window, cx| {
                crate::views::story_privacy::show(state.clone(), crate::views::story_privacy::Page::Lists, None, None, window, cx)
            })
            .unwrap();
        click(&mut cx, "privacy-new-story");
        click(&mut cx, Box::leak(format!("privacy-pick-{MAYA}").into_boxed_str()));
        click(&mut cx, "privacy-picked");
        wait(&mut cx, "naming it", &|cx| privacy.read_with(cx, |p, _| p.page == crate::views::story_privacy::Page::Name));
        cx.update(|window, cx| {
            let name = privacy.read(cx).name.clone();
            name.update(cx, |input, cx| input.set_value("Hikers", window, cx));
        });
        click(&mut cx, "privacy-create");
        wait(&mut cx, "the new story", &|cx| state.read_with(cx, |s, _| !s.custom_story_lists().is_empty()));
        let list = state.read_with(&cx, |s, _| s.custom_story_lists()[0].clone());
        assert_eq!((list.name.as_str(), list.members.as_slice(), list.block_list, list.allows_replies), ("Hikers", [MAYA.to_owned()].as_slice(), false, true));
        // It's listed with the others.
        wait(&mut cx, "its row", &|cx| cx.debug_bounds(Box::leak(format!("privacy-list-{}", list.id).into_boxed_str())).is_some());
        cx.update(gpui_kit::component::WindowExt::close_all_dialogs);

        // Post to it.
        let before = state.read_with(&cx, |s, _| s.my_stories().len());
        state.update(&mut cx, |s, cx| s.create_text_story(cx));
        wait(&mut cx, "the creator", &|cx| cx.debug_bounds("story-creator").is_some());
        let creator = root.read_with(&cx, |r, _| r.story_creator.as_ref().map(|(c, _)| c.clone())).unwrap();
        cx.update(|window, cx| {
            creator.update(cx, |c, cx| {
                c.text.update(cx, |input, cx| input.set_value("Trailhead at 8", window, cx));
                c.choosing = true;
                cx.notify();
            })
        });
        click(&mut cx, Box::leak(format!("story-target-list-{}", list.id).into_boxed_str()));
        creator.update(&mut cx, |c, cx| c.send(cx));
        wait(&mut cx, "the story to post", &|cx| state.read_with(cx, |s, _| s.my_stories().len() == before + 1));
        let posted = state.read_with(&cx, |s, _| s.my_stories().last().cloned().unwrap());
        assert_eq!(posted.recipients.len(), 1);
        assert_eq!((posted.recipients[0].service_id.as_str(), posted.recipients[0].list_ids.as_slice()), (MAYA, [list.id.clone()].as_slice()));

        // Deleted: gone from the lists.
        state.update(&mut cx, |s, cx| s.delete_story_list(list.id.clone(), cx));
        wait(&mut cx, "the list to go", &|cx| state.read_with(cx, |s, _| s.custom_story_lists().is_empty()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn our_story_shows_who_viewed_and_replied(cx: &mut TestAppContext) {
        let (mut cx, state, root, backend, _dir) = demo_window(cx);
        wait(&mut cx, "the demo", &|cx| state.read_with(cx, |s, _| s.my_stories().iter().any(|s| !s.views.is_empty())));
        state.update(&mut cx, |s, cx| s.view_stories(None, cx));
        wait(&mut cx, "the viewer", &|cx| cx.debug_bounds("story-viewer").is_some());
        let viewer = root.read_with(&cx, |r, _| r.story_viewer.as_ref().map(|(v, _)| v.clone())).unwrap();
        click(&mut cx, "story-views");
        wait(&mut cx, "the views", &|cx| cx.debug_bounds("story-views-panel").is_some());
        // Two views and two replies: both tabs.
        assert!(cx.debug_bounds("story-views-tab").is_some() && cx.debug_bounds("story-replies-tab").is_some());
        click(&mut cx, "story-replies-tab");
        assert_eq!(viewer.read_with(&cx, |v, _| v.views_open), Some(crate::views::story_viewer::ViewsTab::Replies));
        // It waits while they're open, and Escape closes them first.
        cx.simulate_keystrokes("escape");
        wait(&mut cx, "the views to close", &|cx| cx.debug_bounds("story-views-panel").is_none());
        assert!(cx.debug_bounds("story-viewer").is_some());
        backend.shutdown();
    }
}
