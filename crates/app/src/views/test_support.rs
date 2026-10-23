//! Helpers for headless UI tests against the demo backend.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, px, size};
use signal_backend::{Backend, Config};

use crate::state::AppState;

/// Polls `done` (redrawing between tries) for up to a few seconds.
pub fn wait(cx: &mut VisualTestContext, what: &str, done: &mut dyn FnMut(&mut VisualTestContext) -> bool) {
    for _ in 0..400 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Clicks a button by its id, once it's drawn.
pub fn press(cx: &mut VisualTestContext, id: &'static str) {
    for _ in 0..300 {
        let found = cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        });
        if found {
            cx.update(|window, cx| window.click(id, cx));
            cx.run_until_parked();
            return;
        }
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("no button {id}");
}

/// Whether an element marked with `debug_selector` is drawn.
pub fn shows(cx: &mut VisualTestContext, what: &'static str) -> bool {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.debug_bounds(what).is_some()
}

/// Clicks the middle of an element marked with `debug_selector`.
pub fn click_at(cx: &mut VisualTestContext, what: &'static str) {
    wait(cx, what, &mut |cx| cx.debug_bounds(what).is_some());
    let at = cx.debug_bounds(what).unwrap().center();
    cx.simulate_click(at, gpui_kit::Modifiers::none());
    cx.run_until_parked();
}

/// [`demo`] with the whole app's window (the root view, which shows the
/// viewers).
pub fn demo_root(cx: &mut TestAppContext) -> (Backend, Entity<AppState>, VisualTestContext, tempfile::TempDir) {
    let (backend, state, dir) = demo_state(cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(1000.), px(800.)) })),
        ..Default::default()
    };
    let (window, _root) = cx
        .update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::root::RootView::new(state, window, cx)))
        })
        .unwrap();
    let cx = VisualTestContext::from_window(window, cx);
    (backend, state, cx, dir)
}

/// The demo backend with its events pumped into the app state.
fn demo_state(cx: &mut TestAppContext) -> (Backend, Entity<AppState>, tempfile::TempDir) {
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

/// The demo backend with its events pumped into the app state as
/// `main.rs` does, and a window (the chat list) to open dialogs in.
pub fn demo(cx: &mut TestAppContext) -> (Backend, Entity<AppState>, VisualTestContext, tempfile::TempDir) {
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
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(800.), px(900.)) })),
        ..Default::default()
    };
    let (window, _sidebar) = cx
        .update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::sidebar::Sidebar::new(state, window, cx)))
        })
        .unwrap();
    let cx = VisualTestContext::from_window(window, cx);
    (backend, state, cx, dir)
}

/// Replaces an input's text as typing does (so it sends its change events).
pub fn type_into(cx: &mut VisualTestContext, input: &Entity<gpui_kit::component::input::InputState>, text: &str) {
    cx.update(|window, cx| input.update(cx, |input, cx| input.focus(window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-a backspace");
    if !text.is_empty() {
        cx.simulate_input(text);
    }
    cx.run_until_parked();
    // Past any pause after typing (the test clock only moves when told).
    cx.executor().advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
}
