//! Signal GPUI: a native Signal Desktop client.

mod assets;
mod avatar_presets;
mod chat_color;
mod audio;
mod emoji_usage;
mod fonts;
mod formatting;
mod image_cache;
mod keys;
mod login_item;
mod main_window;
mod menus;
mod loop_cache;
mod notifications;
mod platform;
mod power;
mod prefs;
mod recorder;
mod reminders;
mod rich_input;
mod shortcuts;
mod sidebar_width;
mod sounds;
mod spelling;
mod ringtone;
mod state;
mod sticker_image;
mod theme;
mod video;
mod video_player;
mod views;
mod window_state;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui_kit::component::Theme;
use gpui_kit::{App, KeyBinding};
use signal_backend::{Backend, Config};


gpui_kit::actions!(signal, [Quit, About, OpenSettings, ShowDebugLog, FocusSearch, SearchInChat, NextConversation, PreviousConversation, CreateStickerPack]);

/// `$SIGNAL_GPUI_DATA_DIR` (a development profile, keyed by a file), or the
/// platform's application data directory (keyed by the Keychain).
fn data_dir() -> Result<(PathBuf, keys::KeyStore)> {
    if let Some(dir) = std::env::var_os("SIGNAL_GPUI_DATA_DIR") {
        return Ok((PathBuf::from(dir), keys::KeyStore::File));
    }
    directories::ProjectDirs::from("org", "signal-gpui", "Signal GPUI")
        .map(|dirs| (dirs.data_dir().to_path_buf(), keys::KeyStore::System))
        .context("no home directory")
}

fn device_name() -> String {
    let host = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    host.unwrap_or_else(|| "Signal GPUI".into())
}

/// Logs to stderr and to daily files in `logs/` (three days kept), which
/// Help → Debug Log collects. Returns the guard that flushes the file writer.
fn init_logging(data_dir: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,presage=info,libsignal_service=warn,sqlx=warn".into())
    };
    let files = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("signal-gpui")
        .filename_suffix("log")
        .max_log_files(3)
        .build(signal_backend::debug_log::log_dir(data_dir));
    let (file_layer, guard) = match files {
        Ok(files) => {
            let (writer, guard) = tracing_appender::non_blocking(files);
            let layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer);
            (Some(layer), Some(guard))
        }
        Err(error) => {
            eprintln!("can't write log files: {error}");
            (None, None)
        }
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter()))
        .with(file_layer.map(|layer| layer.with_filter(filter())))
        .init();

    // Crashes belong in the debug log too.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}\n{}", std::backtrace::Backtrace::force_capture());
        default_hook(info);
    }));
    guard
}

fn start_backend(
    data_dir: PathBuf,
    key_store: keys::KeyStore,
) -> Result<(Backend, futures::channel::mpsc::UnboundedReceiver<signal_backend::Event>)> {
    let db_key = keys::load_or_create(&data_dir, key_store)?;
    tracing::info!("data directory: {}", data_dir.display());
    Backend::start(Config {
        data_dir,
        db_key,
        device_name: device_name(),
        demo: std::env::var_os("SIGNAL_GPUI_DEMO").is_some(),
    })
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, Some("Signal")),
        KeyBinding::new("cmd-f", FocusSearch, Some("Signal")),
        KeyBinding::new("cmd-shift-f", SearchInChat, Some("Signal")),
        KeyBinding::new("alt-down", NextConversation, Some("Signal")),
        KeyBinding::new("alt-up", PreviousConversation, Some("Signal")),
        KeyBinding::new("ctrl-tab", NextConversation, Some("Signal")),
        KeyBinding::new("ctrl-shift-tab", PreviousConversation, Some("Signal")),
        // Desktop's formatting shortcuts.
        KeyBinding::new("cmd-b", views::composer::FormatBold, Some("Composer")),
        KeyBinding::new("cmd-i", views::composer::FormatItalic, Some("Composer")),
        KeyBinding::new("cmd-shift-x", views::composer::FormatStrikethrough, Some("Composer")),
        KeyBinding::new("cmd-e", views::composer::FormatMonospace, Some("Composer")),
        KeyBinding::new("cmd-shift-b", views::composer::FormatSpoiler, Some("Composer")),
        KeyBinding::new("cmd-shift-y", views::composer::RecordVoice, Some("Composer")),
    ]);

    menus::bind_keys(cx);
    shortcuts::bind_keys(cx);
}

fn main() -> Result<()> {
    let (data_dir, key_store) = data_dir()?;
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let _log_guard = init_logging(&data_dir);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    // Parse the emoji index off the main thread while the window opens.
    std::thread::spawn(signal_emoji::warm_up);

    let window_dir = data_dir.clone();
    let (backend, mut events) = start_backend(data_dir, key_store)?;

    // Links macOS opens us with (`sgnl://`, registered in the bundle's
    // Info.plist by scripts/bundle.sh), handed to the app once it runs.
    let (link_tx, mut links) = futures::channel::mpsc::unbounded::<String>();
    let app = gpui_kit::application().with_assets(assets::Assets);
    app.on_open_urls(move |urls| {
        for url in urls {
            let _ = link_tx.unbounded_send(url);
        }
    });
    // Desktop's `activate`: the Dock icon brings the window back.
    app.on_reopen(main_window::show);
    app.run(move |cx| {
        gpui_kit::init(cx);
        fonts::register(cx);
        Theme::sync_system_appearance(None, cx);
        bind_keys(cx);
        menus::install(cx);

        let wake_backend = backend.clone();
        power::on_wake(move || wake_backend.send(signal_backend::Command::Resumed));
        let state = state::new_state(backend, cx);
        cx.on_action(|_: &Quit, cx| main_window::quit(cx));
        // However the app quits (the menu, the Dock, logging out), the
        // databases close first.
        let quit_state = state.clone();
        cx.on_app_quit(move |cx| {
            let state = quit_state.read(cx);
            // A call still up (the Dock's Quit doesn't ask) is hung up.
            if state.call.is_some() {
                state.send(signal_backend::Command::HangUp);
            }
            state.backend.shutdown();
            async {}
        })
        .detach();

        // Desktop's File → Create/upload sticker pack: its own window. It
        // uploads as this account, so only once one is linked.
        let creator_state = state.clone();
        cx.on_action(move |_: &CreateStickerPack, cx| {
            if creator_state.read(cx).account.is_some() {
                views::sticker_creator::open(creator_state.clone(), cx);
            } else if let Some(window) = cx.windows().first() {
                // Desktop's warning (`openArtCreator`, app/main.main.ts).
                use gpui_kit::component::WindowExt as _;
                use gpui_kit::component::notification::Notification;
                let _ = window.update(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::warning("Please set up Signal on your phone and desktop to use the Sticker Pack Creator"),
                        cx,
                    )
                });
            }
        });

        // Pump backend events into the model on the UI thread.
        let pump_state = state.clone();
        cx.spawn(async move |cx| {
            while let Some(event) = events.next().await {
                pump_state.update(cx, |state, cx| state.handle(event, cx));
            }
        })
        .detach();

        // Desktop's `open-url` (app/main.main.ts): the window comes forward
        // and the link opens.
        let link_state = state.clone();
        cx.spawn(async move |cx| {
            while let Some(url) = links.next().await {
                cx.update(|cx| {
                    main_window::show(cx);
                    link_state.update(cx, |state, cx| state.open_link_from_system(url, cx));
                });
            }
        })
        .detach();

        // Opens without taking focus from whatever you're typing in (for
        // screenshots and scripted runs).
        let background = std::env::var_os("SIGNAL_GPUI_BACKGROUND").is_some();
        if let Err(e) = main_window::open(state.clone(), window_dir.clone(), background, cx) {
            tracing::error!("failed to open window: {e:#}");
            cx.quit();
        }
        if !background {
            cx.activate(true);
        }
    });
    Ok(())
}
