//! Platform capabilities.

use std::sync::OnceLock;

/// Newest Emoji version the system color emoji font can draw, so the picker
/// and type-ahead never offer glyphs that would render as tofu.
pub fn max_emoji_version() -> f32 {
    static VERSION: OnceLock<f32> = OnceLock::new();
    *VERSION.get_or_init(detect)
}

#[cfg(target_os = "macos")]
fn detect() -> f32 {
    let output = std::process::Command::new("sw_vers").arg("-productVersion").output();
    let version = output
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let mut parts = version.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    // Apple ships new emoji in x.4 releases.
    match (major, minor) {
        (m, _) if m >= 27 => 17.0,
        (26, n) if n >= 4 => 17.0,
        (26, _) => 16.0,
        (15, n) if n >= 4 => 16.0,
        (15, _) => 15.1,
        (14, n) if n >= 4 => 15.1,
        (14, _) => 15.0,
        (13, n) if n >= 3 => 15.0,
        _ => 14.0,
    }
}

#[cfg(not(target_os = "macos"))]
fn detect() -> f32 {
    15.1
}
