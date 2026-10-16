//! The main window as Signal Desktop keeps it on macOS (app/main.main.ts):
//! closing it hides it and the app keeps running; the Dock icon, the Window
//! menu's Show and links bring it back; Quit ends a call first (after
//! asking, as Desktop's `requestCloseConfirmation` does) and closes the
//! databases. Its size and place are kept between runs
//! (`crate::window_state`).

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Global, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, point, px,
};

use crate::state::AppState;
use crate::views::root::RootView;
use crate::window_state::{self, WindowState};

/// The window's size before it's ever been resized, and the smallest it
/// can be made (this app's layout; Desktop's 800 by 610 and 300 by 200
/// are smaller).
pub const DEFAULT_SIZE: (f32, f32) = (1100., 760.);
pub const MIN_SIZE: (f32, f32) = (720., 480.);

/// Desktop saves the window's stats half a second after the last change
/// (`debounce(saveWindowStats, 500)`).
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// How long Quit waits for a call to end before closing anyway.
const HANG_UP_WAIT: Duration = Duration::from_secs(2);

/// What reopening the window needs.
struct MainWindow {
    state: Entity<AppState>,
    data_dir: PathBuf,
    /// Opens without taking focus (`SIGNAL_GPUI_BACKGROUND`).
    background: bool,
    /// The pending save of the window's size and place.
    save: Option<Task<()>>,
    /// Quitting, after the call (if any) was ended: closing no longer hides.
    quitting: bool,
    /// The window, once open.
    window: Option<AnyWindowHandle>,
}

impl Global for MainWindow {}

/// Opens the main window at its saved size and place (or the default).
pub fn open(state: Entity<AppState>, data_dir: PathBuf, background: bool, cx: &mut App) -> anyhow::Result<()> {
    let saved = window_state::load(&data_dir);
    cx.set_global(MainWindow { state, data_dir, background, save: None, quitting: false, window: None });
    let bounds = initial_bounds(saved, cx);
    open_window(bounds, cx)
}

fn initial_bounds(saved: Option<WindowState>, cx: &App) -> WindowBounds {
    let default = gpui_kit::size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1));
    let primary = cx.primary_display().map(|d| d.bounds());
    match (saved, primary) {
        (Some(saved), Some(primary)) => {
            let displays: Vec<Bounds<gpui_kit::Pixels>> = cx.displays().iter().map(|d| d.bounds()).collect();
            saved.restore(primary, &displays, MIN_SIZE, DEFAULT_SIZE)
        }
        _ => WindowBounds::Windowed(Bounds::centered(None, default, cx)),
    }
}

fn open_window(bounds: WindowBounds, cx: &mut App) -> anyhow::Result<()> {
    let (state, background) = {
        let main = cx.global::<MainWindow>();
        (main.state.clone(), main.background)
    };
    let options = WindowOptions {
        window_bounds: Some(bounds),
        window_min_size: Some(gpui_kit::size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        titlebar: Some(TitlebarOptions {
            title: Some("Signal".into()),
            appears_transparent: true,
            // Centered over the tabs' rail (80 wide; the three buttons
            // span about 59), like Desktop's.
            traffic_light_position: Some(point(px(10.5), px(18.))),
        }),
        app_id: Some("org.signal-gpui".into()),
        focus: !background,
        ..Default::default()
    };
    let (window, _) = gpui_kit::open_window(options, cx, |window, cx| {
        window.observe_window_appearance(crate::prefs::appearance_changed).detach();
        window.on_window_should_close(cx, should_close);
        cx.new(|cx| {
            cx.observe_window_bounds(window, |_, window, cx| schedule_save(window, cx)).detach();
            RootView::new(state.clone(), window, cx)
        })
    })?;
    cx.global_mut::<MainWindow>().window = Some(window);
    Ok(())
}

/// The app's state, once the window has been opened.
pub fn state(cx: &App) -> Option<Entity<AppState>> {
    cx.try_global::<MainWindow>().map(|main| main.state.clone())
}

/// The main window, if it's open (shown or hidden).
fn handle(cx: &App) -> Option<AnyWindowHandle> {
    let window = cx.try_global::<MainWindow>()?.window?;
    cx.windows().contains(&window).then_some(window)
}

/// Desktop's `showWindow`: the window comes forward, opened again if it
/// was closed for good. (Deferred: a shortcut's action runs while its
/// window is being updated, which can't be updated again until it's done.)
pub fn show(cx: &mut App) {
    cx.defer(show_now);
}

fn show_now(cx: &mut App) {
    cx.activate(true);
    match handle(cx) {
        Some(window) => {
            let _ = window.update(cx, |_, window, _| window.activate_window());
        }
        None if cx.has_global::<MainWindow>() => {
            let saved = window_state::load(&cx.global::<MainWindow>().data_dir);
            let bounds = initial_bounds(saved, cx);
            if let Err(error) = open_window(bounds, cx) {
                tracing::error!("failed to open window: {error:#}");
            }
        }
        None => {}
    }
}

/// The window's close button (or Close Window): Desktop on macOS hides
/// the window and keeps running, asking first during a call (and ending
/// it). Returns whether to really close, which only quitting does.
fn should_close(window: &mut Window, cx: &mut App) -> bool {
    if cx.try_global::<MainWindow>().is_none_or(|m| m.quitting) {
        return true;
    }
    let state = cx.global::<MainWindow>().state.clone();
    if in_call(&state, cx) {
        confirm_end_call(window, cx, move |window, cx| {
            state.update(cx, |state, _| state.hang_up());
            hide(window, cx);
        });
    } else {
        hide(window, cx);
    }
    false
}

/// File → Close Window (⌘W): the main window hides as its close button
/// hides it; any other window closes.
pub fn close(window: &mut Window, cx: &mut App) {
    if handle(cx) == Some(window.window_handle()) {
        should_close(window, cx);
    } else {
        window.remove_window();
    }
}

fn in_call(state: &Entity<AppState>, cx: &App) -> bool {
    state.read(cx).call.is_some()
}

/// Hides the window, leaving full screen first as Desktop does (closing a
/// full screen window would leave a black screen behind).
fn hide(window: &mut Window, cx: &mut App) {
    flush_save(window, cx);
    if !window.is_fullscreen() {
        order_out(window);
        return;
    }
    window.toggle_fullscreen();
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        // The animation out of full screen takes most of a second.
        for _ in 0..60 {
            cx.background_executor().timer(Duration::from_millis(50)).await;
            let done = handle.update(cx, |_, window, _| !window.is_fullscreen()).unwrap_or(true);
            if done {
                break;
            }
        }
        let _ = handle.update(cx, |_, window, _| order_out(window));
    })
    .detach();
}

#[cfg(target_os = "macos")]
fn order_out(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
        // SAFETY: an AppKit window handle's `ns_view` is the window's live
        // NSView, and this runs on the main thread.
        let view: &objc2_app_kit::NSView = unsafe { appkit.ns_view.cast().as_ref() };
        if let Some(ns_window) = view.window() {
            ns_window.orderOut(None);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn order_out(window: &Window) {
    window.minimize_window();
}

/// Quit (the menu or ⌘Q): during a call Desktop asks "Close Signal and end
/// the call?"; the call is hung up and given a moment to end before the app
/// quits. The databases close as it quits (`on_app_quit` in main.rs).
/// Deferred, as `show` is.
pub fn quit(cx: &mut App) {
    cx.defer(quit_deferred);
}

fn quit_deferred(cx: &mut App) {
    let Some(main) = cx.try_global::<MainWindow>() else {
        cx.quit();
        return;
    };
    let state = main.state.clone();
    if !in_call(&state, cx) {
        quit_now(cx);
        return;
    }
    show_now(cx);
    let Some(window) = handle(cx) else {
        quit_now(cx);
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        confirm_end_call(window, cx, |_, cx| {
            let state = cx.global::<MainWindow>().state.clone();
            state.update(cx, |state, _| state.hang_up());
            cx.spawn(async move |cx| {
                let mut waited = Duration::ZERO;
                while waited < HANG_UP_WAIT && cx.update(|cx| in_call(&state, cx)) {
                    cx.background_executor().timer(Duration::from_millis(50)).await;
                    waited += Duration::from_millis(50);
                }
                cx.update(quit_now);
            })
            .detach();
        });
    });
}

fn quit_now(cx: &mut App) {
    if let Some(window) = handle(cx) {
        let _ = window.update(cx, |_, window, cx| flush_save(window, cx));
    }
    if cx.has_global::<MainWindow>() {
        cx.global_mut::<MainWindow>().quitting = true;
    }
    cx.quit();
}

/// Desktop's in-call close confirmation (`requestCloseConfirmation`).
fn confirm_end_call(window: &mut Window, cx: &mut App, confirmed: impl Fn(&mut Window, &mut App) + 'static) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    use gpui_kit::{ParentElement as _};
    if window.has_active_dialog(cx) {
        window.close_all_dialogs(cx);
    }
    let confirmed = std::rc::Rc::new(confirmed);
    window.open_dialog(cx, move |dialog, _, _| {
        let confirmed = confirmed.clone();
        dialog.title("Close Signal and end the call?").w(px(400.)).footer(
            DialogFooter::new()
                .child(
                    Button::new("close-not-now")
                        .label("Not Now")
                        .outline()
                        .on_click(|_, window, cx| window.close_all_dialogs(cx)),
                )
                .child(Button::new("close-confirm").label("Close").danger().on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    confirmed(window, cx);
                })),
        )
    });
}

/// Saves the window's size and place half a second after it last changed.
fn schedule_save(window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        let _ = handle.update(cx, |_, window, cx| flush_save(window, cx));
    });
    if cx.has_global::<MainWindow>() {
        cx.global_mut::<MainWindow>().save = Some(task);
    }
}

/// Saves now (on hiding and quitting, as Desktop's `debouncedSaveStats.flush()`).
fn flush_save(window: &mut Window, cx: &mut App) {
    if !cx.has_global::<MainWindow>() {
        return;
    }
    let main = cx.global_mut::<MainWindow>();
    main.save = None;
    let state = WindowState::capture(window.window_bounds(), window.is_maximized());
    window_state::save(&main.data_dir, &state);
}

#[cfg(test)]
pub(crate) mod tests {
    use gpui_kit::TestAppContext;

    use super::*;
    use crate::views::test_support::wait;

    /// The demo backend pumped into the state, and the main window opened
    /// from it, saving to `<dir>/window`.
    pub(crate) fn open_demo(
        cx: &mut TestAppContext,
    ) -> (signal_backend::Backend, Entity<AppState>, AnyWindowHandle, tempfile::TempDir) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = signal_backend::Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = signal_backend::Backend::start(config).unwrap();
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
        let data_dir = dir.path().join("window");
        std::fs::create_dir_all(&data_dir).unwrap();
        cx.update(|cx| open(state.clone(), data_dir, true, cx)).unwrap();
        let window = cx.update(|cx| handle(cx)).expect("the main window");
        (backend, state, window, dir)
    }

    #[gpui_kit::test]
    fn closing_hides_and_quitting_asks_during_a_call(cx: &mut TestAppContext) {
        let (backend, state, window, dir) = open_demo(cx);
        let data_dir = dir.path().join("window");
        let mut vcx = gpui_kit::VisualTestContext::from_window(window, cx);
        let vcx = &mut vcx;

        // Closing keeps the window (hidden) and saves where it was.
        let closes = vcx.update(should_close);
        assert!(!closes);
        assert!(vcx.update(|_, cx| handle(cx)).is_some());
        assert!(window_state::load(&data_dir).is_some(), "saved on hiding");

        // During a call, quitting asks first; "Not Now" keeps everything.
        wait(vcx, "the account", &mut |cx| state.read_with(cx, |s, _| s.account.is_some()));
        let maya = signal_store::ConversationId("d:00000000-0000-4000-8000-00000000a11c".into());
        state.update(vcx, |s, cx| {
            s.open_call_lobby(maya, false, cx);
            s.start_call(cx);
        });
        wait(vcx, "the call", &mut |cx| state.read_with(cx, |s, _| s.call.is_some()));
        vcx.update(|_, cx| quit(cx));
        crate::views::test_support::press(vcx, "close-not-now");
        assert!(state.read_with(vcx, |s, _| s.call.is_some()), "still in the call");
        assert!(!vcx.update(|_, cx| cx.global::<MainWindow>().quitting));
        // "Close" hangs up, then quits.
        vcx.update(|_, cx| quit(cx));
        crate::views::test_support::press(vcx, "close-confirm");
        wait(vcx, "the call to end", &mut |cx| state.read_with(cx, |s, _| s.call.is_none()));
        wait(vcx, "quitting", &mut |cx| {
            // Past the wait for the call to end (the test clock only moves when told).
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.update(|_, cx| cx.global::<MainWindow>().quitting)
        });
        backend.shutdown();
    }
}
