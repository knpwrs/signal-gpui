//! The app was called Signal GPUI (bundle ID `org.signal-gpui`) before it
//! was Ziggy. On first launch its data directory moves to Ziggy's; the
//! database key's Keychain item is copied by `keys`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// The data directory the app had as Signal GPUI.
pub fn former_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "signal-gpui", "Signal GPUI").map(|dirs| dirs.data_dir().to_path_buf())
}

/// What became of Signal GPUI's data, for the log once it's open.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// There was none.
    Nothing,
    /// It moved here from this directory.
    From(PathBuf),
    /// It's still in this directory: Ziggy's existed already, so it was
    /// left for the user to sort out rather than merged.
    LeftIn(PathBuf),
    /// Signal GPUI is running, with its databases open.
    StillRunning,
}

/// Moves Signal GPUI's data directory to `data_dir` when there is one and
/// `data_dir` doesn't exist yet. Refuses while Signal GPUI runs, with an
/// alert: moving its databases from under it would split the data.
pub fn move_data(data_dir: &Path) -> Result<Moved> {
    let Some(former) = former_data_dir() else { return Ok(Moved::Nothing) };
    let moved = move_from(&former, data_dir, former_app_running)?;
    if moved == Moved::StillRunning {
        alert(
            "Quit Signal GPUI to open Ziggy",
            "Signal GPUI is now Ziggy. Ziggy moves your chats over the first time it opens, \
             which it can't do while Signal GPUI is running. Quit Signal GPUI, then open Ziggy again.",
        );
        bail!("Signal GPUI is still running; quit it before opening Ziggy");
    }
    Ok(moved)
}

fn move_from(former: &Path, data_dir: &Path, running: impl Fn() -> bool) -> Result<Moved> {
    if !former.exists() {
        return Ok(Moved::Nothing);
    }
    if data_dir.exists() {
        return Ok(Moved::LeftIn(former.to_owned()));
    }
    if running() {
        return Ok(Moved::StillRunning);
    }
    if let Some(parent) = data_dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // Both are in Application Support, on one volume: a rename, all at once.
    std::fs::rename(former, data_dir)
        .with_context(|| format!("moving {} to {}", former.display(), data_dir.display()))?;
    Ok(Moved::From(former.to_owned()))
}

/// Whether Signal GPUI is running: its executable was `signal-gpui`, in a
/// bundle or not.
fn former_app_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "signal-gpui"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "macos")]
fn alert(message: &str, detail: &str) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSApplication};
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    // In front, as nothing else of ours is open yet. (`activate` needs
    // macOS 14; the app runs on 13.)
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.runModal();
}

#[cfg(not(target_os = "macos"))]
fn alert(message: &str, detail: &str) {
    eprintln!("{message}: {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_directory_moves_once() {
        let root = tempfile::tempdir().unwrap();
        let former = root.path().join("org.signal-gpui.Signal-GPUI");
        let data_dir = root.path().join("new").join("io.github.knpwrs.Ziggy");
        std::fs::create_dir_all(former.join("logs")).unwrap();
        std::fs::write(former.join("app.db"), "chats").unwrap();

        assert_eq!(move_from(&former, &data_dir, || false).unwrap(), Moved::From(former.clone()));
        assert!(!former.exists());
        assert_eq!(std::fs::read_to_string(data_dir.join("app.db")).unwrap(), "chats");
        assert!(data_dir.join("logs").is_dir());
        // Next launch: nothing left to move.
        assert_eq!(move_from(&former, &data_dir, || false).unwrap(), Moved::Nothing);
    }

    #[test]
    fn nothing_moves_while_the_old_app_runs_or_over_new_data() {
        let root = tempfile::tempdir().unwrap();
        let former = root.path().join("old");
        let data_dir = root.path().join("new");
        std::fs::create_dir_all(&former).unwrap();
        std::fs::write(former.join("app.db"), "chats").unwrap();

        assert_eq!(move_from(&former, &data_dir, || true).unwrap(), Moved::StillRunning);
        assert!(former.join("app.db").exists() && !data_dir.exists());

        std::fs::create_dir_all(&data_dir).unwrap();
        assert_eq!(move_from(&former, &data_dir, || false).unwrap(), Moved::LeftIn(former.clone()));
        assert!(former.join("app.db").exists());
    }
}
