//! Each Settings page, headless with the demo backend.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowBounds, WindowOptions, px, size};
use signal_backend::{Backend, Config};

use super::{Page, Settings};
use crate::state::AppState;

/// Waits (a few seconds at most) for `done`.
pub(crate) fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) -> bool {
    (0..300).any(|_| {
        cx.run_until_parked();
        let ok = done(cx);
        if !ok {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        ok
    })
}

pub(crate) struct Harness {
    pub backend: Backend,
    pub state: Entity<AppState>,
    pub window: AnyWindowHandle,
    pub settings: Entity<Settings>,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// The demo account, its settings loaded, with Settings open at `page`.
    pub fn open(cx: &mut TestAppContext, page: Page) -> Self {
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
        // The demo account has read receipts on once its settings load.
        assert!(wait(cx, |cx| state.read_with(cx, |s, _| s.read_receipts)), "demo settings didn't load");
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(1000.), px(900.)) })),
            ..Default::default()
        };
        let (window, _sidebar) = cx
            .update(|cx| {
                let state = state.clone();
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::sidebar::Sidebar::new(state, window, cx)))
            })
            .unwrap();
        let window: AnyWindowHandle = window;
        let settings = cx
            .update_window(window, |_, window, cx| super::show(state.clone(), Page::General, window, cx))
            .unwrap()
            .expect("settings open");
        cx.run_until_parked();
        let harness = Self { backend, state, window, settings, _dir: dir };
        if page != Page::General {
            harness.click(cx, page.id());
        }
        assert_eq!(cx.read(|cx| harness.settings.read(cx).page), page);
        harness
    }

    pub fn click(&self, cx: &mut TestAppContext, id: &'static str) {
        cx.update_window(self.window, |_, window, cx| window.click(id, cx)).unwrap();
        cx.run_until_parked();
    }

    pub fn shown(&self, cx: &mut TestAppContext, id: &'static str) -> bool {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    /// A button's label, or a switch's state as "on"/"off".
    pub fn value(&self, cx: &mut TestAppContext, id: &'static str) -> Option<String> {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            let found = window.try_find(id)?;
            match found.checked() {
                Some(on) => Some(if on { "on" } else { "off" }.to_owned()),
                None => found.label().map(str::to_owned),
            }
        })
        .unwrap()
    }

    /// A stored setting.
    pub fn stored(&self, key: &'static str) -> Option<String> {
        let db = self.backend.db().clone();
        futures::executor::block_on(self.backend.spawn(async move { db.setting(key).await })).ok().flatten()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.backend.shutdown();
    }
}

#[gpui_kit::test]
fn pages_are_listed_and_open_from_the_list(cx: &mut TestAppContext) {
    let h = Harness::open(cx, Page::General);
    // Desktop's list starts with our profile, which opens the profile editor.
    assert!(h.shown(cx, "edit-profile"));
    for page in Page::LIST {
        h.click(cx, page.id());
        assert_eq!(cx.read(|cx| h.settings.read(cx).page), page);
        assert!(h.shown(cx, "settings-content"));
    }
}

#[gpui_kit::test]
fn general_shows_the_account_and_opens_at_login(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicBool, Ordering};
    // A stand-in login item: nothing is registered with the system.
    static ON: AtomicBool = AtomicBool::new(false);
    let h = Harness::open(cx, Page::General);
    h.state.update(cx, |s, _| {
        s.login_item = crate::login_item::LoginItem { is_enabled: || ON.load(Ordering::SeqCst), set: |on| {
            ON.store(on, Ordering::SeqCst);
            Ok(())
        } }
    });
    assert!(h.shown(cx, "unlink") && h.shown(cx, "debug-log") && h.shown(cx, "about"));
    assert_eq!(h.value(cx, "open-at-login").as_deref(), Some("off"));
    h.click(cx, "open-at-login");
    assert!(ON.load(Ordering::SeqCst));
    assert_eq!(h.value(cx, "open-at-login").as_deref(), Some("on"));
    h.click(cx, "open-at-login");
    assert!(!ON.load(Ordering::SeqCst));
}

#[gpui_kit::test]
fn appearance_sets_the_theme_and_zoom(cx: &mut TestAppContext) {
    use crate::prefs::ThemeSetting;
    use gpui_kit::component::ActiveTheme as _;
    let h = Harness::open(cx, Page::Appearance);
    assert_eq!(h.value(cx, "theme").as_deref(), Some("System"));
    assert_eq!(h.value(cx, "zoom").as_deref(), Some("100%"));
    // The menus' choices call these.
    h.state.update(cx, |s, cx| s.set_theme(ThemeSetting::Dark, cx));
    h.state.update(cx, |s, cx| s.set_zoom(1.25, cx));
    assert_eq!(h.value(cx, "theme").as_deref(), Some("Dark"));
    assert_eq!(h.value(cx, "zoom").as_deref(), Some("125%"));
    assert!(cx.read(|cx| cx.theme().is_dark()));
    assert_eq!(cx.read(|cx| cx.theme().font_size), px(20.));
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_THEME).as_deref() == Some("dark")));
    assert!(wait(cx, |_| h.stored(crate::prefs::ZOOM_KEY).as_deref() == Some("1.25")));
    h.state.update(cx, |s, cx| s.set_theme(ThemeSetting::Light, cx));
    assert!(!cx.read(|cx| cx.theme().is_dark()));
    // Zoom stays when the theme changes.
    assert_eq!(cx.read(|cx| cx.theme().font_size), px(20.));
}

#[gpui_kit::test]
fn notifications_turn_off_and_back_on(cx: &mut TestAppContext) {
    use crate::notifications::NotificationSetting;
    let h = Harness::open(cx, Page::Notifications);
    assert_eq!(h.value(cx, "notifications-enabled").as_deref(), Some("on"));
    assert_eq!(h.value(cx, "notification-content").as_deref(), Some("Name, content, and actions"));
    h.click(cx, "notifications-enabled");
    assert_eq!(cx.read(|cx| h.state.read(cx).notification_setting), NotificationSetting::Off);
    assert!(wait(cx, |_| h.stored(NotificationSetting::KEY).as_deref() == Some("off")));
    h.click(cx, "notifications-enabled");
    assert_eq!(cx.read(|cx| h.state.read(cx).notification_setting), NotificationSetting::NameAndMessage);
}

#[gpui_kit::test]
fn calls_list_the_devices(cx: &mut TestAppContext) {
    let h = Harness::open(cx, Page::Calls);
    assert!(wait(cx, |cx| h.state.read_with(cx, |s, _| s.call_devices.is_some())));
    for id in ["call-camera", "call-microphone", "call-speaker"] {
        assert!(h.shown(cx, id), "{id}");
    }
    // Desktop's defaults: calls ring, with sound, not relayed.
    assert_eq!(h.value(cx, "incoming-calls").as_deref(), Some("on"));
    assert_eq!(h.value(cx, "calling-sounds").as_deref(), Some("on"));
    assert_eq!(h.value(cx, "always-relay-calls").as_deref(), Some("off"));
    for id in ["incoming-calls", "calling-sounds", "always-relay-calls"] {
        h.click(cx, id);
    }
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_INCOMING_CALLS).as_deref() == Some("false")
        && h.stored("call_sounds").as_deref() == Some("false")
        && h.stored(signal_backend::SETTING_ALWAYS_RELAY_CALLS).as_deref() == Some("true")));
    assert_eq!(h.value(cx, "always-relay-calls").as_deref(), Some("on"));
}

#[gpui_kit::test]
fn privacy_toggles_turn_read_receipts_and_typing_off(cx: &mut TestAppContext) {
    let h = Harness::open(cx, Page::Privacy);
    // The demo account has both on.
    assert!(cx.read(|cx| h.state.read(cx).typing_indicators));
    h.click(cx, "read-receipts");
    h.click(cx, "typing-indicators");
    assert!(
        wait(cx, |_| h.stored(signal_backend::SETTING_READ_RECEIPTS).as_deref() == Some("false")
            && h.stored(signal_backend::SETTING_TYPING_INDICATORS).as_deref() == Some("false")),
        "the settings weren't saved"
    );
    // And they stay off once the backend says they changed.
    assert!(wait(cx, |cx| h.state.read_with(cx, |s, _| !s.read_receipts && !s.typing_indicators)));
    // Story view receipts, unset, follow read receipts.
    assert!(!cx.read(|cx| h.state.read(cx).story_view_receipts_on()));
    assert_eq!(h.value(cx, "stories-toggle").as_deref(), Some("Turn off stories"));
}

#[gpui_kit::test]
fn data_usage_shows_image_optimization(cx: &mut TestAppContext) {
    let h = Harness::open(cx, Page::DataUsage);
    assert_eq!(h.value(cx, "image-optimization").as_deref(), Some("Maximum"));
}

#[gpui_kit::test]
fn chats_toggles_save_and_contacts_import(cx: &mut TestAppContext) {
    use crate::state::ContactSync;
    use signal_backend::AccountSetting;
    let h = Harness::open(cx, Page::Chats);
    // The demo account generates link previews; the rest start at Desktop's defaults.
    assert_eq!(h.value(cx, "link-previews").as_deref(), Some("on"));
    assert_eq!(h.value(cx, "keep-muted-chats-archived").as_deref(), Some("off"));
    assert_eq!(h.value(cx, "address-book-photos").as_deref(), Some("off"));
    assert_eq!(h.value(cx, "auto-convert-emoji").as_deref(), Some("on"));
    h.click(cx, "keep-muted-chats-archived");
    h.click(cx, "address-book-photos");
    h.click(cx, "link-previews");
    h.click(cx, "auto-convert-emoji");
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_KEEP_MUTED_CHATS_ARCHIVED).as_deref() == Some("true")
        && h.stored(signal_backend::SETTING_PREFER_CONTACT_AVATARS).as_deref() == Some("true")
        && h.stored(signal_backend::SETTING_LINK_PREVIEWS).as_deref() == Some("false")
        && h.stored(crate::prefs::AUTO_CONVERT_EMOJI_KEY).as_deref() == Some("false")));
    assert!(cx.read(|cx| h.state.read(cx).prefs.flag(AccountSetting::KeepMutedChatsArchived)));
    assert_eq!(h.value(cx, "auto-convert-emoji").as_deref(), Some("off"));

    // "Import now": the demo phone answers at once, and the time shows.
    assert_eq!(cx.read(|cx| h.state.read(cx).prefs.contacts_synced_at), None);
    h.click(cx, "import-contacts");
    assert!(wait(cx, |cx| h.state.read_with(cx, |s, _| s.contact_sync == ContactSync::Idle && s.prefs.contacts_synced_at.is_some())));
}

#[gpui_kit::test]
fn address_book_photos_come_first_when_preferred(cx: &mut TestAppContext) {
    use signal_backend::AccountSetting;
    let h = Harness::open(cx, Page::Chats);
    let dir = tempfile::tempdir().unwrap();
    let (profile, contact) = (dir.path().join("profile.png"), dir.path().join("contact.png"));
    // A made-up person with both photos, and one with only an address book photo.
    let db = h.backend.db().clone();
    let (p, c) = (profile.to_string_lossy().into_owned(), contact.to_string_lossy().into_owned());
    let json = format!(r#"{{"person-a":"{c}","person-b":"{c}"}}"#);
    futures::executor::block_on(h.backend.spawn(async move {
        db.set_person_avatar("person-a", &p).await?;
        db.set_setting(signal_backend::SETTING_CONTACT_AVATARS, &json).await
    }))
    .unwrap();
    h.state.update(cx, |s, cx| s.reload_conversations(cx));
    let photo = |cx: &mut TestAppContext, sid: &str| cx.read(|cx| h.state.read(cx).photo_of(sid).map(|p| p.to_path_buf()));
    assert!(wait(cx, |cx| photo(cx, "person-b").is_some()));
    assert_eq!(photo(cx, "person-a").as_deref(), Some(profile.as_path()));
    h.state.update(cx, |s, cx| s.set_account_setting(AccountSetting::PreferContactAvatars, true, cx));
    assert_eq!(photo(cx, "person-a").as_deref(), Some(contact.as_path()));
    assert_eq!(photo(cx, "person-b").as_deref(), Some(contact.as_path()));
}
