//! The main window's size, position and maximized or full screen state,
//! kept between runs as Signal Desktop keeps them (`captureWindowStats` and
//! `createWindow` in app/main.main.ts): saved half a second after the window
//! last moved or resized, in a small file beside the databases (Desktop's
//! `ephemeralConfig`, not its database), and restored only where it would
//! still be visible.

use std::path::{Path, PathBuf};

use gpui_kit::{Bounds, Pixels, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};

/// Desktop's `BOUNDS_BUFFER`: how much of the window must stay on a screen.
const BOUNDS_BUFFER: f32 = 100.;

/// What's saved (Desktop's `windowConfigSchema`, less the menu bar).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub fullscreen: bool,
}

impl WindowState {
    /// The window as it is now: its bounds (the size to come back to when
    /// it's in full screen) and whether it's maximized.
    pub fn capture(bounds: WindowBounds, maximized: bool) -> Self {
        let (frame, fullscreen) = match bounds {
            WindowBounds::Windowed(frame) | WindowBounds::Maximized(frame) => (frame, false),
            WindowBounds::Fullscreen(frame) => (frame, true),
        };
        Self {
            x: f32::from(frame.origin.x),
            y: f32::from(frame.origin.y),
            width: f32::from(frame.size.width),
            height: f32::from(frame.size.height),
            maximized: maximized || matches!(bounds, WindowBounds::Maximized(_)),
            fullscreen,
        }
    }

    /// How to open the window: the saved size within the primary screen
    /// (Desktop's `Math.min(windowConfig.width, maxWidth)`), at least
    /// `min`; the saved position if it's still on a screen (Desktop's
    /// `isVisible`), centered otherwise.
    pub fn restore(&self, primary: Bounds<Pixels>, displays: &[Bounds<Pixels>], min: (f32, f32), default: (f32, f32)) -> WindowBounds {
        let valid = |v: f32| v.is_finite() && v > 0.;
        let width = if valid(self.width) && self.width >= min.0 { self.width } else { default.0 };
        let height = if valid(self.height) && self.height >= min.1 { self.height } else { default.1 };
        let width = width.min(f32::from(primary.size.width)).max(min.0);
        let height = height.min(f32::from(primary.size.height)).max(min.1);
        let placed = Bounds { origin: point(px(self.x), px(self.y)), size: size(px(width), px(height)) };
        let frame = if self.x.is_finite() && self.y.is_finite() && displays.iter().any(|d| is_visible(placed, *d)) {
            placed
        } else {
            centered(primary, width, height)
        };
        if self.fullscreen {
            WindowBounds::Fullscreen(frame)
        } else if self.maximized {
            WindowBounds::Maximized(frame)
        } else {
            WindowBounds::Windowed(frame)
        }
    }
}

fn centered(display: Bounds<Pixels>, width: f32, height: f32) -> Bounds<Pixels> {
    let x = f32::from(display.origin.x) + (f32::from(display.size.width) - width) / 2.;
    let y = f32::from(display.origin.y) + (f32::from(display.size.height) - height) / 2.;
    Bounds { origin: point(px(x), px(y)), size: size(px(width), px(height)) }
}

/// Desktop's `isVisible`: 100 pixels of the window left or right of a
/// screen's edges, and its top on the screen with 100 pixels below it.
fn is_visible(window: Bounds<Pixels>, display: Bounds<Pixels>) -> bool {
    let (wx, wy) = (f32::from(window.origin.x), f32::from(window.origin.y));
    let ww = f32::from(window.size.width);
    let (dx, dy) = (f32::from(display.origin.x), f32::from(display.origin.y));
    let (dw, dh) = (f32::from(display.size.width), f32::from(display.size.height));
    wx + ww >= dx + BOUNDS_BUFFER && wx <= dx + dw - BOUNDS_BUFFER && wy >= dy && wy <= dy + dh - BOUNDS_BUFFER
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("window.json")
}

/// The saved window, if there is one and it reads.
pub fn load(data_dir: &Path) -> Option<WindowState> {
    let text = std::fs::read_to_string(path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Saves the window (written to a temporary file and renamed, so a quit
/// mid-write leaves the last one).
pub fn save(data_dir: &Path, state: &WindowState) {
    let target = path(data_dir);
    let partial = target.with_extension("json.partial");
    let written = serde_json::to_vec(state)
        .map_err(std::io::Error::other)
        .and_then(|json| std::fs::write(&partial, json))
        .and_then(|()| std::fs::rename(&partial, &target));
    if let Err(error) = written {
        tracing::warn!(%error, "couldn't save the window's size and position");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds { origin: point(px(x), px(y)), size: size(px(w), px(h)) }
    }

    const MIN: (f32, f32) = (720., 480.);
    const DEFAULT: (f32, f32) = (1100., 760.);

    #[test]
    fn a_saved_window_comes_back_where_it_was() {
        let screen = rect(0., 0., 1920., 1080.);
        let saved = WindowState::capture(WindowBounds::Windowed(rect(200., 100., 900., 700.)), false);
        assert_eq!(saved.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Windowed(rect(200., 100., 900., 700.)));
        let maximized = WindowState::capture(WindowBounds::Windowed(rect(0., 25., 1920., 1055.)), true);
        assert_eq!(maximized.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Maximized(rect(0., 25., 1920., 1055.)));
        let full = WindowState::capture(WindowBounds::Fullscreen(rect(10., 30., 1000., 800.)), false);
        assert_eq!(full.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Fullscreen(rect(10., 30., 1000., 800.)));
    }

    #[test]
    fn a_window_off_every_screen_is_centered() {
        let screen = rect(0., 0., 1920., 1080.);
        // A second screen to the right, since unplugged.
        let saved = WindowState { x: 2500., y: 100., width: 900., height: 700., maximized: false, fullscreen: false };
        assert_eq!(saved.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Windowed(rect(510., 190., 900., 700.)));
        // Still there: kept.
        let second = rect(1920., 0., 1440., 900.);
        assert_eq!(saved.restore(screen, &[screen, second], MIN, DEFAULT), WindowBounds::Windowed(rect(2500., 100., 900., 700.)));
        // Too little showing on the left, or the top above the screen.
        let left = WindowState { x: -850., ..saved };
        assert!(matches!(left.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Windowed(b) if b.origin.x == px(510.)));
        let above = WindowState { x: 100., y: -20., ..saved };
        assert!(matches!(above.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Windowed(b) if b.origin.x == px(510.)));
    }

    #[test]
    fn sizes_fit_the_screen_and_the_minimum() {
        let screen = rect(0., 0., 1280., 800.);
        let huge = WindowState { x: 0., y: 0., width: 3000., height: 2000., maximized: false, fullscreen: false };
        assert_eq!(huge.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Windowed(rect(0., 0., 1280., 800.)));
        let tiny = WindowState { width: 100., height: f32::NAN, ..huge };
        assert_eq!(tiny.restore(screen, &[screen], MIN, DEFAULT), WindowBounds::Windowed(rect(0., 0., 1100., 760.)));
    }

    #[test]
    fn saved_to_a_file_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
        let state = WindowState { x: 12., y: 34., width: 900., height: 700., maximized: true, fullscreen: false };
        save(dir.path(), &state);
        assert_eq!(load(dir.path()), Some(state));
        // Desktop's older configs had no maximized/fullscreen flags.
        std::fs::write(dir.path().join("window.json"), r#"{"x":1,"y":2,"width":800,"height":610}"#).unwrap();
        assert_eq!(load(dir.path()).map(|s| (s.maximized, s.fullscreen)), Some((false, false)));
        std::fs::write(dir.path().join("window.json"), "not json").unwrap();
        assert_eq!(load(dir.path()), None);
    }
}
