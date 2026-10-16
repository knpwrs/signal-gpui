//! Each Settings page, headless with the demo backend.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowBounds, WindowOptions, px, size};
use signal_backend::{Backend, Config};

use super::{Page, Settings};
use crate::state::AppState;

/// Waits (ten seconds at most, for a busy test machine) for `done`.
pub(crate) fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) -> bool {
    (0..1000).any(|_| {
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
    pub fn value(&self, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> Option<String> {
        let id = id.into();
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            let found = window.try_find(id.clone())?;
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

#[gpui_kit::test]
fn notification_sounds_and_reactions_save(cx: &mut TestAppContext) {
    use crate::prefs::Toggle;
    let h = Harness::open(cx, Page::Notifications);
    // Desktop's defaults: reactions notify; no sounds; no reminders.
    assert_eq!(h.value(cx, "reaction-notifications").as_deref(), Some("on"));
    assert_eq!(h.value(cx, "unread-reminders").as_deref(), Some("off"));
    assert_eq!(h.value(cx, "notification-sounds").as_deref(), Some("off"));
    assert_eq!(h.value(cx, "message-sounds").as_deref(), Some("off"));
    for id in ["reaction-notifications", "unread-reminders", "notification-sounds", "message-sounds"] {
        h.click(cx, id);
    }
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_REACTION_NOTIFICATIONS).as_deref() == Some("false")
        && h.stored(signal_backend::SETTING_SHOW_UNREAD_REMINDERS).as_deref() == Some("true")
        && h.stored("notification_sounds").as_deref() == Some("true")
        && h.stored("message_sounds").as_deref() == Some("true")));
    assert!(cx.read(|cx| h.state.read(cx).prefs.on(Toggle::MessageSounds)));
}

#[gpui_kit::test]
fn while_muted_opens_from_notifications_and_saves(cx: &mut TestAppContext) {
    let h = Harness::open(cx, Page::Notifications);
    h.click(cx, "while-muted");
    assert_eq!(cx.read(|cx| h.settings.read(cx).page), Page::WhileMuted);
    assert_eq!(h.value(cx, "muted-calls").as_deref(), Some("off"));
    assert_eq!(h.value(cx, "muted-mentions").as_deref(), Some("on"));
    assert_eq!(h.value(cx, "muted-replies").as_deref(), Some("on"));
    h.click(cx, "muted-calls");
    h.click(cx, "muted-replies");
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_NOTIFY_CALLS_IF_MUTED).as_deref() == Some("true")
        && h.stored(signal_backend::SETTING_NOTIFY_REPLIES_IF_MUTED).as_deref() == Some("false")));
    let summary = cx.read(|cx| super::notifications::while_muted_summary(&h.state.read(cx).prefs));
    assert_eq!(summary, "Calls, mentions");
    // Back to Notifications, which the list still shows as chosen.
    h.click(cx, "settings-back");
    assert_eq!(cx.read(|cx| h.settings.read(cx).page), Page::Notifications);
}

#[test]
fn muted_chats_let_through_what_desktop_does() {
    use crate::state::{Mentions, allowed_while_muted};
    let prefs = crate::prefs::Prefs::default();
    let mention = Mentions { mentions_me: true, replies_to_me: false };
    let reply = Mentions { mentions_me: false, replies_to_me: true };
    assert!(allowed_while_muted(false, mention, &prefs) && allowed_while_muted(false, reply, &prefs));
    assert!(!allowed_while_muted(false, Mentions::default(), &prefs));
    // Never a muted 1:1 chat.
    assert!(!allowed_while_muted(true, mention, &prefs));
    let mut off = prefs.clone();
    off.flags.insert(signal_backend::AccountSetting::NotifyRepliesIfMuted, false);
    assert!(!allowed_while_muted(false, reply, &off) && allowed_while_muted(false, mention, &off));
    assert_eq!(crate::state::reaction_text("Maya", "👍", "See you at 9"), "Maya reacted 👍 to: See you at 9");
}

#[gpui_kit::test]
fn an_unread_muted_chat_is_due_a_reminder(cx: &mut TestAppContext) {
    use signal_store::{ChatState, ConversationId, DeliveryStatus, Direction, NewMessage};
    let h = Harness::open(cx, Page::Notifications);
    let now = chrono::Utc::now().timestamp_millis();
    let day = 24 * 60 * 60 * 1000;
    // A made-up muted group with two unread messages from five days ago,
    // and reminders started a week ago.
    let chat = ConversationId("g:0000000000000000000000000000000000000000000000000000000000000003".into());
    let db = h.backend.db().clone();
    let id = chat.clone();
    futures::executor::block_on(h.backend.spawn(async move {
        db.set_setting(crate::reminders::ENABLED_AT_KEY, &(now - 7 * day).to_string()).await?;
        for (n, author) in [(1, "made-up-a"), (2, "made-up-b")] {
            let at = now - 5 * day + n;
            db.insert_message(&NewMessage {
                conversation_id: id.clone(),
                sent_at: at,
                received_at: at,
                direction: Direction::Incoming,
                author: Some(author.into()),
                author_name: None,
                body: "old news".into(),
                status: DeliveryStatus::Delivered,
                quote: None,
                attachments: Vec::new(),
                link_preview: None,
                expires_in: 0,
            })
            .await?;
        }
        db.apply_chat_states(&[ChatState { id: id.clone(), archived: false, muted_until: i64::MAX, marked_unread: false, blocked: false }])
            .await
    }))
    .unwrap();
    h.state.update(cx, |s, cx| s.reload_conversations(cx));
    assert!(wait(cx, |cx| h.state.read_with(cx, |s, _| s.conversation(&chat).is_some_and(|c| c.muted && c.unread_count >= 2))));
    let pick = |cx: &mut TestAppContext| {
        let picked = h.state.read_with(cx, |s, _| s.pick_unread_reminder(now));
        futures::executor::block_on(picked)
    };
    // Off by default.
    assert_eq!(pick(cx), None);
    h.click(cx, "unread-reminders");
    let (due, data) = pick(cx).expect("a reminder is due");
    assert_eq!(due, chat);
    // (The demo's own unread messages there count too.)
    assert!(data.senders.iter().any(|s| s == "made-up-b"), "{:?}", data.senders);
    assert!(data.unread >= 2);
}

#[gpui_kit::test]
fn privacy_sets_the_default_timer_and_who_sees_the_number(cx: &mut TestAppContext) {
    let h = Harness::open(cx, Page::Privacy);
    assert_eq!(h.value(cx, "universal-timer").as_deref(), Some("Off"));
    h.state.update(cx, |s, cx| s.set_universal_timer(86_400, cx));
    assert_eq!(h.value(cx, "universal-timer").as_deref(), Some("1 day"));
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_UNIVERSAL_EXPIRE_TIMER).as_deref() == Some("86400")));
    // A custom time shows as itself.
    h.state.update(cx, |s, cx| s.set_universal_timer(3 * 3600, cx));
    assert_eq!(h.value(cx, "universal-timer").as_deref(), Some("3 hours"));

    h.click(cx, "phone-number-privacy");
    assert_eq!(cx.read(|cx| h.settings.read(cx).page), Page::PhoneNumber);
    // Desktop's default: nobody sees it, and nobody finds us by it.
    let prefs = |cx: &mut TestAppContext| cx.read(|cx| (h.state.read(cx).prefs.number_shared, h.state.read(cx).prefs.number_discoverable));
    assert_eq!(prefs(cx), (false, false));
    h.click(cx, "pnp-find-everyone");
    assert_eq!(prefs(cx), (false, true));
    h.click(cx, "pnp-see-everyone");
    assert_eq!(prefs(cx), (true, true));
    assert!(wait(cx, |_| h.stored(signal_backend::SETTING_PHONE_NUMBER_SHARING).as_deref() == Some("true")
        && h.stored(signal_backend::SETTING_PHONE_NUMBER_DISCOVERABLE).as_deref() == Some("true")));
    // Nobody can't be chosen while everybody sees the number.
    h.click(cx, "pnp-find-nobody");
    assert_eq!(prefs(cx), (true, true));
    h.click(cx, "pnp-see-nobody");
    assert_eq!(prefs(cx), (false, true));
}

#[gpui_kit::test]
fn blocked_lists_and_unblocks(cx: &mut TestAppContext) {
    use signal_store::ConversationId;
    let h = Harness::open(cx, Page::Privacy);
    let sam = ConversationId("d:00000000-0000-4000-8000-0000000000e0".into());
    h.state.read_with(cx, |s, _| s.set_chat(sam.clone(), signal_backend::ChatChange::Blocked(true)));
    assert!(wait(cx, |cx| h.state.read_with(cx, |s, _| s.blocked().len() == 1)));
    h.click(cx, "blocked");
    assert_eq!(cx.read(|cx| h.settings.read(cx).page), Page::Blocked);
    assert_eq!(h.value(cx, ("unblock", 0usize)).as_deref(), Some("Unblock"));
    cx.update_window(h.window, |_, window, cx| window.click(("unblock", 0usize), cx)).unwrap();
    assert!(wait(cx, |cx| h.state.read_with(cx, |s, _| s.blocked().is_empty())));
}
