//! Help → Debug Log (Signal Desktop's debug log window): the redacted log
//! with system info, which can be saved to a file or submitted to
//! debuglogs.org for a link to share.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::prelude::*;
use gpui_kit::{App, ClipboardItem, Entity, SharedString, Window, div, px};
use signal_backend::debug_log;

use crate::state::AppState;
use crate::theme::Palette;

/// Lines shown before "Save or submit to include all of it" (Desktop shows
/// enough to make clear it scrolls, not the whole thing).
const PREVIEW_LINES: usize = 300;

enum Phase {
    Loading,
    Ready,
    Submitting,
    Submitted(SharedString),
}

pub struct DebugLog {
    backend: signal_backend::Backend,
    phase: Phase,
    /// The whole redacted log, once collected.
    log: Option<Arc<String>>,
    preview: SharedString,
}

/// From the Help menu: unless another dialog is up.
pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    if !window.has_active_dialog(cx) {
        show(state, window, cx);
    }
}

/// On top of whatever is open (e.g. from Settings).
pub fn show(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let backend = state.read(cx).backend.clone();
    let model = cx.new(|_| DebugLog { backend, phase: Phase::Loading, log: None, preview: SharedString::default() });
    collect(&state, &model, cx);

    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let model = model.clone();
        let this = model.read(cx);
        let title = match this.phase {
            Phase::Submitted(_) => "Debug log submitted",
            _ => "Submit debug log",
        };
        let body = match &this.phase {
            Phase::Loading => status("Collecting the log…", palette),
            Phase::Submitting => status("Submitting…", palette),
            Phase::Ready => ready(&model, this.preview.clone(), palette),
            Phase::Submitted(link) => submitted(link.clone(), palette),
        };
        dialog.title(title).w(px(640.)).child(body)
    });
}

/// Assembles the log off the UI thread.
fn collect(state: &Entity<AppState>, model: &Entity<DebugLog>, cx: &mut App) {
    let task = assemble(state, cx);
    let model = model.clone();
    cx.spawn(async move |cx| {
        let log = task.await;
        let preview = preview_of(&log);
        model.update(cx, |this, cx| {
            this.log = Some(Arc::new(log));
            this.preview = preview.into();
            this.phase = Phase::Ready;
            cx.notify();
        });
    })
    .detach();
}

/// The redacted log, assembled off the UI thread.
pub fn assemble(state: &Entity<AppState>, cx: &mut App) -> gpui_kit::Task<String> {
    let state = state.read(cx);
    let data_dir = state.backend.data_dir().to_path_buf();
    let mut user = Vec::new();
    if let Some(account) = &state.account {
        user.push(("Account".to_owned(), account.aci.clone()));
        if let Some(phone) = &account.phone {
            user.push(("Phone number".to_owned(), phone.clone()));
        }
    }
    user.push(("Conversations".to_owned(), state.conversations.len().to_string()));
    let header = debug_log::Header {
        app_version: env!("CARGO_PKG_VERSION").into(),
        os_version: os_version(),
        arch: std::env::consts::ARCH.into(),
        user,
    };
    // Our own folders and the home directory never leave the machine.
    let mut sensitive = vec![data_dir.clone()];
    if let Some(home) = std::env::var_os("HOME") {
        sensitive.push(PathBuf::from(home));
    }
    cx.background_spawn(async move { debug_log::assemble(&debug_log::log_dir(&data_dir), &header, &sensitive) })
}

fn preview_of(log: &str) -> String {
    let mut lines = log.lines();
    let head: Vec<&str> = lines.by_ref().take(PREVIEW_LINES).collect();
    let mut preview = head.join("\n");
    if lines.next().is_some() {
        preview.push_str("\n\n…\n\nThis is the start of the log. Saving or submitting includes all of it.");
    }
    preview
}

fn os_version() -> String {
    let version = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    match version {
        Some(version) => format!("{} {version}", std::env::consts::OS),
        None => std::env::consts::OS.into(),
    }
}

fn status(text: &'static str, palette: Palette) -> gpui_kit::AnyElement {
    div().py_8().flex().justify_center().text_color(palette.text_secondary).child(text).into_any_element()
}

fn ready(model: &Entity<DebugLog>, preview: SharedString, palette: Palette) -> gpui_kit::AnyElement {
    let (save, submit) = (model.clone(), model.clone());
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(div().text_sm().text_color(palette.text_secondary).child(
            "Debug logs help diagnose problems. Phone numbers, account and group ids, and file paths are \
             redacted, and they never include message contents.",
        ))
        .child(
            div()
                .id("debug-log-text")
                .debug_selector(|| "debug-log-text".into())
                .h(px(360.))
                .overflow_y_scroll()
                .p_2()
                .rounded_md()
                .bg(palette.input_bg)
                .font_family("Menlo")
                .text_xs()
                .child(preview),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(Button::new("save-debug-log").label("Save…").on_click(move |_, _, cx| save_log(&save, cx)))
                .child(
                    Button::new("submit-debug-log")
                        .primary()
                        .label("Submit")
                        .on_click(move |_, window, cx| submit_log(&submit, window, cx)),
                ),
        )
        .into_any_element()
}

fn submitted(link: SharedString, palette: Palette) -> gpui_kit::AnyElement {
    let copy = link.clone();
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_sm()
                .text_color(palette.text_secondary)
                .child("Share this link with whoever is helping you investigate the problem."),
        )
        .child(div().p_2().rounded_md().bg(palette.input_bg).font_family("Menlo").text_sm().child(link))
        .child(div().flex().justify_end().child(Button::new("copy-debug-log-link").primary().label("Copy link").on_click(
            move |_, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                window.push_notification(Notification::new().message("Link copied"), cx);
            },
        )))
        .into_any_element()
}

fn save_log(model: &Entity<DebugLog>, cx: &mut App) {
    let Some(log) = model.read(cx).log.clone() else { return };
    let directory = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_default();
    let name = format!("signal-gpui-debug-log-{}.txt", chrono::Local::now().format("%Y-%m-%d-%H%M%S"));
    let chosen = cx.prompt_for_new_path(&directory, Some(&name));
    cx.spawn(async move |_| {
        if let Ok(Ok(Some(path))) = chosen.await
            && let Err(error) = std::fs::write(&path, log.as_bytes())
        {
            tracing::warn!(%error, "failed to save debug log");
        }
    })
    .detach();
}

fn submit_log(model: &Entity<DebugLog>, window: &mut Window, cx: &mut App) {
    let Some(log) = model.read(cx).log.clone() else { return };
    let backend = model.read(cx).backend.clone();
    model.update(cx, |this, cx| {
        this.phase = Phase::Submitting;
        cx.notify();
    });
    let upload = backend.spawn(async move { debug_log::upload(&log, env!("CARGO_PKG_VERSION")).await });
    let model = model.clone();
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = upload.await;
        model.update(cx, |this, cx| {
            match result {
                Ok(link) => this.phase = Phase::Submitted(link.into()),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "debug log upload failed");
                    this.phase = Phase::Ready;
                    let message = "Couldn't submit the debug log. Check your connection, or save it instead.";
                    let _ = window_handle.update(cx, |_, window, cx| {
                        window.push_notification(Notification::error(message), cx);
                    });
                }
            }
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use gpui_kit::{TestAppContext, VisualTestContext, WindowOptions};
    use signal_backend::{Backend, Config};

    use super::*;

    #[test]
    fn preview_is_the_start_of_the_log() {
        let log: String = (0..PREVIEW_LINES + 10).map(|i| format!("line {i}\n")).collect();
        let preview = preview_of(&log);
        assert!(preview.starts_with("line 0\nline 1"));
        assert!(!preview.contains(&format!("line {PREVIEW_LINES}\n")));
        assert!(preview.ends_with("Saving or submitting includes all of it."));
        assert_eq!(preview_of("short"), "short");
    }

    #[gpui_kit::test]
    fn help_menu_collects_the_log(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/signal-gpui.2026-10-14.log"), "INFO hello\n").unwrap();
        let (backend, _events) = Backend::start(Config {
            data_dir: dir.path().to_owned(),
            db_key: [7; 32],
            device_name: "test".into(),
            demo: true,
        })
        .unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (window, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| gpui_kit::Empty)).unwrap()
        });
        cx.update_window(window, |_, window, cx| open(state.clone(), window, cx)).unwrap();
        let mut cx = VisualTestContext::from_window(window, cx);
        let mut shown = None;
        for _ in 0..300 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            shown = cx.debug_bounds("debug-log-text");
            if shown.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(shown.is_some(), "the collected log never showed");
        backend.shutdown();
    }
}
