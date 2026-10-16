//! Settings kept in the app database that Settings shows: this app's own
//! (Desktop keeps them per device, unsynced) and the account's synced ones
//! as the backend saved them.

use std::collections::{BTreeMap, HashMap};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Window, px};
use signal_backend::AccountSetting;

/// Desktop's theme choice (`theme-setting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeSetting {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeSetting {
    pub const ALL: [ThemeSetting; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    pub fn as_setting(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Desktop's labels (`themeSystem`, `themeLight`, `themeDark`).
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// Settings key: the zoom factor (Desktop's `zoomFactor`), as a number.
pub const ZOOM_KEY: &str = "zoom_factor";

/// Desktop's zoom choices (`DEFAULT_ZOOM_FACTORS`).
pub const ZOOM_FACTORS: [f32; 7] = [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0];

/// "75%", "100%"…
pub fn zoom_label(factor: f32) -> String {
    format!("{}%", (factor * 100.).round() as i64)
}

/// The base text size the theme starts from; zoom scales it.
const BASE_FONT_SIZE: f32 = 16.;

/// What the theme follows, for the window's appearance observer.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThemeChoice(pub ThemeSetting);

impl Global for ThemeChoice {}

/// Applies a theme choice and zoom factor to every window: light or dark
/// as chosen (System follows macOS), and the base text size scaled.
/// Zoom scales text and anything sized in rems; sizes given in pixels
/// stay, where Desktop's zoom scales the whole page.
pub fn apply(theme: ThemeSetting, zoom: f32, cx: &mut App) {
    cx.set_global(ThemeChoice(theme));
    match theme {
        ThemeSetting::System => Theme::sync_system_appearance(None, cx),
        ThemeSetting::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
    let size = px(BASE_FONT_SIZE * zoom.clamp(0.5, 3.));
    if Theme::global(cx).font_size != size {
        Theme::update(cx, |theme| theme.font_size = size);
    }
}

/// macOS switched light and dark: the theme follows unless one was chosen.
pub fn appearance_changed(window: &mut Window, cx: &mut App) {
    let choice = cx.try_global::<ThemeChoice>().copied().unwrap_or_default();
    if choice.0 == ThemeSetting::System {
        let font_size = Theme::global(cx).font_size;
        Theme::sync_system_appearance(Some(window), cx);
        Theme::update(cx, |theme| theme.font_size = font_size);
    }
}

/// Settings key: `"false"` to leave typed emoticons as they are (Desktop's
/// `autoConvertEmoji`, on until turned off).
pub const AUTO_CONVERT_EMOJI_KEY: &str = "auto_convert_emoji";

/// Every key [`Prefs`] reads.
pub const KEYS: &[&str] =
    &[signal_backend::SETTING_THEME, ZOOM_KEY, AUTO_CONVERT_EMOJI_KEY, signal_backend::SETTING_CONTACTS_SYNCED_AT];

/// The settings as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    pub theme: ThemeSetting,
    pub zoom: f32,
    /// The account's on/off settings as stored, defaults filled in.
    pub flags: BTreeMap<AccountSetting, bool>,
    /// Typed emoticons become emoji (":-)" → 🙂).
    pub auto_convert_emoji: bool,
    /// When the phone's contacts last came (unix millis).
    pub contacts_synced_at: Option<i64>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { theme: ThemeSetting::System, zoom: 1., flags: BTreeMap::new(), auto_convert_emoji: true, contacts_synced_at: None }
    }
}

impl Prefs {
    /// From the stored values of [`KEYS`] and the account settings' keys.
    pub fn from_values(values: &HashMap<String, Option<String>>) -> Self {
        let get = |key: &str| values.get(key).and_then(|v| v.as_deref());
        let flags = AccountSetting::ALL.iter().map(|s| (*s, s.is_on(get(s.key())))).collect();
        Self {
            theme: ThemeSetting::from_setting(get(signal_backend::SETTING_THEME)),
            zoom: get(ZOOM_KEY).and_then(|z| z.parse().ok()).filter(|z: &f32| z.is_finite() && *z > 0.).unwrap_or(1.),
            flags,
            auto_convert_emoji: get(AUTO_CONVERT_EMOJI_KEY) != Some("false"),
            contacts_synced_at: get(signal_backend::SETTING_CONTACTS_SYNCED_AT).and_then(|t| t.parse().ok()),
        }
    }

    /// Whether an account setting is on (its default until loaded).
    pub fn flag(&self, setting: AccountSetting) -> bool {
        self.flags.get(&setting).copied().unwrap_or_else(|| setting.default_on())
    }

    /// Every key to read.
    pub fn keys() -> impl Iterator<Item = &'static str> {
        KEYS.iter().copied().chain(AccountSetting::ALL.iter().map(|s| s.key()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_values_read_with_desktops_defaults() {
        let prefs = Prefs::from_values(&HashMap::new());
        assert_eq!((prefs.theme, prefs.zoom), (ThemeSetting::System, 1.));
        assert!(prefs.auto_convert_emoji && prefs.contacts_synced_at.is_none());
        assert!(prefs.flags[&AccountSetting::ReactionNotifications] && !prefs.flags[&AccountSetting::NotifyCallsIfMuted]);

        let values: HashMap<String, Option<String>> = [
            (signal_backend::SETTING_THEME, "dark"),
            (ZOOM_KEY, "1.25"),
            (signal_backend::SETTING_REACTION_NOTIFICATIONS, "false"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), Some(v.to_owned())))
        .collect();
        let prefs = Prefs::from_values(&values);
        assert_eq!((prefs.theme, prefs.zoom), (ThemeSetting::Dark, 1.25));
        assert!(!prefs.flags[&AccountSetting::ReactionNotifications]);
        for theme in ThemeSetting::ALL {
            assert_eq!(ThemeSetting::from_setting(Some(theme.as_setting())), theme);
        }
        assert_eq!(ZOOM_FACTORS.map(zoom_label), ["75%", "90%", "100%", "110%", "125%", "150%", "200%"]);
    }
}
