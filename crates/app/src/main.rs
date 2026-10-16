//! Signal GPUI: a native Signal Desktop client.

mod assets;
mod audio;
mod emoji_usage;
mod fonts;
mod formatting;
mod image_cache;
mod keys;
mod loop_cache;
mod notifications;
mod platform;
mod recorder;
mod sidebar_width;
mod ringtone;
mod state;
mod sticker_image;
mod theme;
mod video;
mod video_player;
mod views;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui_kit::component::Theme;
use gpui_kit::{
    App, AppContext as _, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, point, px,
    size,
};
use signal_backend::{Backend, Config};

use crate::views::root::RootView;

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

    cx.set_menus(vec![Menu {
        name: "Signal GPUI".into(),
        items: vec![
            MenuItem::action("About Signal GPUI", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit Signal GPUI", Quit),
        ],
        disabled: false,
    }, Menu {
        name: "File".into(),
        items: vec![MenuItem::action("Create/upload sticker pack", CreateStickerPack)],
        disabled: false,
    }, Menu {
        name: "Help".into(),
        items: vec![MenuItem::action("Debug Log…", ShowDebugLog)],
        disabled: false,
    }]);
}

fn main() -> Result<()> {
    let (data_dir, key_store) = data_dir()?;
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let _log_guard = init_logging(&data_dir);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    // Parse the emoji index off the main thread while the window opens.
    std::thread::spawn(signal_emoji::warm_up);

    let (backend, mut events) = start_backend(data_dir, key_store)?;

    gpui_kit::application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        fonts::register(cx);
        Theme::sync_system_appearance(None, cx);
        bind_keys(cx);

        let state = state::new_state(backend, cx);
        let quit_state = state.clone();
        cx.on_action(move |_: &Quit, cx| {
            quit_state.read(cx).backend.shutdown();
            cx.quit();
        });

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

        // Opens without taking focus from whatever you're typing in (for
        // screenshots and scripted runs).
        let background = std::env::var_os("SIGNAL_GPUI_BACKGROUND").is_some();
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.), px(480.))),
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
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            window.observe_window_appearance(|window, cx| Theme::sync_system_appearance(Some(window), cx))
                .detach();
            cx.new(|cx| RootView::new(state.clone(), window, cx))
        });
        if let Err(e) = opened {
            tracing::error!("failed to open window: {e:#}");
            cx.quit();
        }
        if !background {
            cx.activate(true);
        }
    });
    Ok(())
}
