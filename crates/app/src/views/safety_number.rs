//! Desktop's safety number view: the 60 digits and a QR code to compare
//! with the other person's device, "Mark as verified", and automatic key
//! verification (key transparency), with Desktop's states and words
//! (`ts/components/SafetyNumberViewer.dom.tsx`,
//! `KeyTransparencyOnboardingDialog`, `KeyTransparencyErrorDialog`).

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::prelude::*;
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::{App, Entity, FontWeight, Window, div, px, white};
use signal_backend::Command;
use signal_store::ConversationId;

use crate::state::{AppState, KtStatus};
use crate::theme::Palette;

const QR_SIZE: f32 = 180.;

/// Desktop's `KEY_TRANSPARENCY_URL`.
const KEY_TRANSPARENCY_URL: &str = "https://support.signal.org/hc/articles/10223569377562";
/// Desktop's `createSupportUrl`, in English.
const SUPPORT_REQUEST_URL: &str = "https://support.signal.org/hc/en-us/requests/new?desktop";

/// `conversation` is their 1:1 chat's id (whether or not the chat exists);
/// `name` is theirs.
pub fn open(state: Entity<AppState>, conversation: ConversationId, name: String, window: &mut Window, cx: &mut App) {
    state.update(cx, |state, _| {
        state.safety_number = None;
        state.close_safety_number();
    });
    state.read(cx).send(Command::LoadSafetyNumber { conversation: conversation.clone() });
    let closing = state.clone();
    let introduce = state.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let loaded = state
            .read(cx)
            .safety_number
            .as_ref()
            .filter(|(id, _)| *id == conversation)
            .map(|(_, result)| result.clone());
        let body = match loaded {
            None => div().flex().justify_center().py_6().child(Spinner::new()).into_any_element(),
            Some(Err(error)) => div().text_sm().text_color(palette.text_secondary).child(error).into_any_element(),
            Some(Ok(number)) => {
                let kt = key_transparency(&state, &conversation, &name, number.auto_verify, cx);
                let (state, id, verified) = (state.clone(), conversation.clone(), number.verified);
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_4()
                    .child(
                        // Black on white whatever the theme: phones scan this.
                        div().p_3().rounded_lg().bg(white()).child(crate::views::link::qr_code(&number.qr, QR_SIZE)),
                    )
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_lg()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(blocks(&number.digits).chunks(4).map(|row| {
                                div().flex().gap_4().children(row.iter().map(|block| div().child(block.clone())))
                            })),
                    )
                    .child(
                        div().text_sm().text_color(palette.text_secondary).text_center().child(format!(
                            "To verify end-to-end encryption with {}, compare the numbers above with their device. \
                             They can also scan your code with their device.",
                            name
                        )),
                    )
                    .when(verified, |d| {
                        d.child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!("✓ {} is verified", name)))
                    })
                    .child(
                        Button::new("toggle-verified")
                            .label(if verified { "Clear verification" } else { "Mark as verified" })
                            .when(!verified, |b| b.primary())
                            .when(verified, |b| b.outline())
                            .on_click(move |_, _, cx| {
                                state.read(cx).send(Command::SetVerified { conversation: id.clone(), verified: !verified });
                            }),
                    )
                    .children(kt)
                    .into_any_element()
            }
        };
        let closing = closing.clone();
        dialog
            .title("Safety number")
            .w(px(420.))
            .child(body)
            .on_close(move |_, _, cx| closing.update(cx, |state, _| state.close_safety_number()))
    });
    // Desktop's `toggleSafetyNumberModal`: the introduction, once.
    let s = introduce.read(cx);
    if signal_backend::key_transparency_on() && !s.key_transparency_disabled && !s.kt_onboarding_seen {
        onboarding(introduce, window, cx);
    }
}

/// The key transparency section (Desktop's `KeyTransparency`); none when
/// it's off.
fn key_transparency(
    state: &Entity<AppState>,
    conversation: &ConversationId,
    name: &str,
    auto_verify: signal_backend::AutoVerify,
    cx: &App,
) -> Option<gpui_kit::AnyElement> {
    use signal_backend::AutoVerify;
    let palette = Palette::get(cx);
    let status = match auto_verify {
        AutoVerify::Off => return None,
        AutoVerify::Unavailable => KtStatus::Unavailable,
        AutoVerify::Available => state
            .read(cx)
            .kt_status
            .as_ref()
            .filter(|(id, _)| id == conversation)
            .map_or(KtStatus::Idle, |(_, status)| *status),
    };
    let (label, icon) = button_text(status);
    let (state, id, name) = (state.clone(), conversation.clone(), name.to_owned());
    let mut button = Button::new("kt-verify")
        .w_full()
        .outline()
        .label(label)
        .loading(status == KtStatus::Running)
        .disabled(status == KtStatus::Running)
        .on_click(move |_, window, cx| match status {
            KtStatus::Idle => state.update(cx, |s, cx| s.check_key_transparency(id.clone(), cx)),
            KtStatus::Running => {}
            other => popup(other, &name, window, cx),
        });
    if let Some(icon) = icon {
        button = button.icon(icon);
    }
    Some(
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().mt_4().font_weight(FontWeight::MEDIUM).child("Automatic key verification"))
            .child(button)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .text_color(palette.text_secondary)
                    .child("Auto-verification is not available for all chats.")
                    .child(
                        Button::new("kt-learn-more")
                            .link()
                            .label("Learn more")
                            .on_click(|_, _, cx| cx.open_url(KEY_TRANSPARENCY_URL)),
                    ),
            )
            .into_any_element(),
    )
}

/// The button's words and icon in each state. (Desktop shows a key while
/// idle; there's no key icon here.)
fn button_text(status: KtStatus) -> (&'static str, Option<IconName>) {
    match status {
        KtStatus::Idle => ("Verify automatically", None),
        KtStatus::Running => ("Verifying encryption...", None),
        KtStatus::Ok => ("Encryption verified", Some(IconName::CircleCheck)),
        KtStatus::Fail | KtStatus::Unavailable => ("Auto-verification unavailable", Some(IconName::Info)),
    }
}

/// What a finished check means (Desktop's `Popup`).
fn popup_text(status: KtStatus, name: &str) -> (&'static str, String) {
    match status {
        KtStatus::Ok => (
            "Encryption was auto-verified for this chat",
            "For people you’re connected to by phone number, Signal can automatically confirm whether the \
             connection is secure using a process called key transparency. For added security, verify \
             end-to-end encryption manually by comparing the numbers on the previous screen or scanning the \
             code on their device."
                .into(),
        ),
        // The same title as a failure, as Desktop has it.
        KtStatus::Unavailable => (
            "Auto-verification is not available for this chat",
            "Signal can only automatically verify the encryption in chats where you’re connected to someone \
             via a phone number. If the chat was started with a username or a group in common, verify \
             end-to-end encryption by comparing the numbers on the previous screen or scanning the code on \
             their device."
                .into(),
        ),
        _ => (
            "Auto-verification is not available for this chat",
            format!(
                "Signal can not automatically verify the encryption for this chat. This could be because {name} \
                 changed their phone number or their phone number privacy settings. Verify end-to-end \
                 encryption manually by comparing the numbers on the previous screen or scanning the code on \
                 their device."
            ),
        ),
    }
}

fn popup(status: KtStatus, name: &str, window: &mut Window, cx: &mut App) {
    let (title, body) = popup_text(status, name);
    window.open_alert_dialog(cx, move |alert, _, _| alert.title(title).description(body.clone()).ok_text("Okay"));
}

/// Desktop's `KeyTransparencyOnboardingDialog`.
fn onboarding(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, seen) = (state.clone(), state.clone());
        alert
            .title("Signal can now auto-verify encryption")
            .description(
                "For contacts you’re connected to by phone number, Signal can automatically confirm whether \
                 the connection is secure using a process called key transparency. For added security, you can \
                 still verify connections manually using a QR code or by comparing safety numbers.",
            )
            .confirm()
            .ok_text("Continue")
            .cancel_text("Learn more")
            .on_ok(move |_, _, cx| {
                state.update(cx, |s, cx| s.set_kt_onboarding_seen(cx));
                true
            })
            .on_cancel(move |_, _, cx| {
                seen.update(cx, |s, cx| s.set_kt_onboarding_seen(cx));
                cx.open_url(KEY_TRANSPARENCY_URL);
                true
            })
    });
}

/// Desktop's `KeyTransparencyErrorDialog`, when our own account's check
/// starts failing. "Submit" opens Signal's support form, with a link to an
/// uploaded debug log when "Include debug log" is ticked (Desktop's
/// `createSupportUrl` with `kt`); nothing is sent otherwise.
pub fn show_error(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let share = cx.new(|_| false);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let shared = *share.read(cx);
        let (toggle, view_state, submit_state, submit_share) = (share.clone(), state.clone(), state.clone(), share.clone());
        dialog
            .title("Automatic Key Verification is currently unavailable for your device. Submit a report to help us improve?")
            .w(px(440.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_sm().text_color(palette.text_secondary).child(
                        "Including your debug logs help us diagnose and fix the issue, and do not contain \
                         identifying information. Sharing is optional.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                gpui_kit::component::checkbox::Checkbox::new("kt-share-log")
                                    .label("Include debug log")
                                    .checked(shared)
                                    .on_click(move |on, _, cx| {
                                        toggle.update(cx, |v, cx| {
                                            *v = *on;
                                            cx.notify();
                                        })
                                    }),
                            )
                            .child(Button::new("kt-view-log").small().ghost().label("View").on_click(
                                move |_, window, cx| crate::views::debug_log::show(view_state.clone(), window, cx),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("kt-error-cancel")
                                    .label("No thanks")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(Button::new("kt-error-submit").primary().label("Submit").on_click(
                                move |_, window, cx| {
                                    let with_log = *submit_share.read(cx);
                                    submit_report(&submit_state, with_log, cx);
                                    window.close_dialog(cx);
                                },
                            )),
                    ),
            )
    });
}

/// Opens the support form, with an uploaded debug log's link when chosen.
fn submit_report(state: &Entity<AppState>, with_log: bool, cx: &mut App) {
    if !with_log {
        cx.open_url(SUPPORT_REQUEST_URL);
        return;
    }
    let backend = state.read(cx).backend.clone();
    let log = crate::views::debug_log::assemble(state, cx);
    cx.spawn(async move |cx| {
        let log = log.await;
        let upload = backend.spawn(async move { signal_backend::debug_log::upload(&log, env!("CARGO_PKG_VERSION")).await });
        let url = match upload.await {
            Ok(link) => support_url(Some(&link)),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "debug log upload failed");
                support_url(None)
            }
        };
        cx.update(|cx| cx.open_url(&url));
    })
    .detach();
}

/// Desktop's `createSupportUrl({ query: { kt: link } })`.
fn support_url(debug_log: Option<&str>) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(b).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    match debug_log {
        Some(link) => format!("{SUPPORT_REQUEST_URL}&kt={}", encode(link)),
        None => SUPPORT_REQUEST_URL.to_owned(),
    }
}

/// Groups of five, as Signal shows them.
fn blocks(digits: &str) -> Vec<String> {
    digits.as_bytes().chunks(5).map(|chunk| String::from_utf8_lossy(chunk).into_owned()).collect()
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowBounds, WindowOptions, px, size};
    use signal_backend::{Backend, Config};
    use signal_store::{Conversation, ConversationId};

    use crate::state::{AppState, KtStatus};

    const MAYA: &str = "d:00000000-0000-4000-8000-00000000a11c";
    const JORDAN: &str = "d:00000000-0000-4000-8000-0000000000b0";

    /// The demo backend and a window, with backend events going into the
    /// model as the app sends them.
    fn start(cx: &mut TestAppContext) -> (Backend, Entity<AppState>, AnyWindowHandle, tempfile::TempDir) {
        cx.update(gpui_kit::init);
        // The demo backend answers from its own Tokio threads.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true, former_data_dir: None };
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
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(800.), px(900.)) })),
            ..Default::default()
        };
        let (window, _sidebar) = cx
            .update(|cx| {
                let state = state.clone();
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::sidebar::Sidebar::new(state, window, cx)))
            })
            .unwrap();
        (backend, state, window, dir)
    }

    fn demo_chat(backend: &Backend, id: &str) -> Conversation {
        (0..300)
            .find_map(|_| {
                let (db, id) = (backend.db().clone(), ConversationId(id.into()));
                let found = futures::executor::block_on(backend.spawn(async move { db.conversation(&id).await })).ok().flatten();
                if found.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                found
            })
            .expect("the demo has them")
    }

    /// Runs until `done` holds (or fails after about three seconds).
    fn wait(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        for _ in 0..300 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{what}");
    }

    fn open(cx: &mut TestAppContext, state: &Entity<AppState>, window: AnyWindowHandle, chat: Conversation) {
        cx.update_window(window, |_, window, cx| super::open(state.clone(), chat.id, chat.title, window, cx)).unwrap();
        wait(cx, "no safety number", |cx| state.read_with(cx, |s, _| matches!(s.safety_number, Some((_, Ok(_))))));
    }

    #[gpui_kit::test]
    fn shows_the_number_and_marks_verified(cx: &mut TestAppContext) {
        let (backend, state, window, _dir) = start(cx);
        let maya = demo_chat(&backend, MAYA);
        // Past Desktop's introduction to automatic key verification.
        state.update(cx, |s, cx| s.set_kt_onboarding_seen(cx));
        open(cx, &state, window, maya);
        let number = state.read_with(cx, |s, _| s.safety_number.clone().unwrap().1.unwrap());
        assert_eq!((number.digits.len(), number.verified), (60, false));
        assert!(cx.update_window(window, |_, window, cx| window.has_active_dialog(cx)).unwrap());

        cx.update_window(window, |_, window, cx| window.click("toggle-verified", cx)).unwrap();
        wait(cx, "Mark as verified didn't take", |cx| {
            state.read_with(cx, |s, _| matches!(&s.safety_number, Some((_, Ok(n))) if n.verified))
        });
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn verifies_automatically_as_desktop_shows_it(cx: &mut TestAppContext) {
        let (backend, state, window, _dir) = start(cx);
        let (maya, jordan) = (demo_chat(&backend, MAYA), demo_chat(&backend, JORDAN));
        // Desktop's introduction comes with the first safety number.
        assert!(!state.read_with(cx, |s, _| s.kt_onboarding_seen));
        state.update(cx, |s, cx| s.set_kt_onboarding_seen(cx));

        open(cx, &state, window, maya.clone());
        let number = state.read_with(cx, |s, _| s.safety_number.clone().unwrap().1.unwrap());
        assert_eq!(number.auto_verify, signal_backend::AutoVerify::Available);
        cx.update_window(window, |_, window, cx| window.click("kt-verify", cx)).unwrap();
        assert_eq!(state.read_with(cx, |s, _| s.kt_status.clone()), Some((maya.id.clone(), KtStatus::Running)));
        // The demo verifies Maya's key.
        wait(cx, "Maya's key didn't verify", |cx| {
            state.read_with(cx, |s, _| s.kt_status == Some((maya.id.clone(), KtStatus::Ok)))
        });

        // Another screen starts over; the demo can't verify Jordan's.
        cx.update_window(window, |_, window, cx| window.close_all_dialogs(cx)).unwrap();
        open(cx, &state, window, jordan.clone());
        assert_eq!(state.read_with(cx, |s, _| s.kt_status.clone()), None);
        cx.update_window(window, |_, window, cx| window.click("kt-verify", cx)).unwrap();
        wait(cx, "Jordan's check didn't fail", |cx| {
            state.read_with(cx, |s, _| s.kt_status == Some((jordan.id.clone(), KtStatus::Fail)))
        });

        // Turned off in Settings: the section goes.
        state.update(cx, |s, cx| s.set_key_transparency_disabled(true, cx));
        cx.update_window(window, |_, window, cx| window.close_all_dialogs(cx)).unwrap();
        open(cx, &state, window, maya);
        wait(cx, "still offered", |cx| {
            state.read_with(cx, |s, _| {
                matches!(&s.safety_number, Some((_, Ok(n))) if n.auto_verify == signal_backend::AutoVerify::Off)
            })
        });
        backend.shutdown();
    }

    #[test]
    fn desktops_words_for_each_state() {
        assert_eq!(super::button_text(KtStatus::Idle).0, "Verify automatically");
        assert_eq!(super::button_text(KtStatus::Running).0, "Verifying encryption...");
        assert_eq!(super::button_text(KtStatus::Ok).0, "Encryption verified");
        assert_eq!(super::button_text(KtStatus::Fail).0, super::button_text(KtStatus::Unavailable).0);
        let (title, body) = super::popup_text(KtStatus::Fail, "Maya Chen");
        assert_eq!(title, super::popup_text(KtStatus::Unavailable, "Maya Chen").0);
        assert!(body.contains("because Maya Chen changed their phone number"));
        assert!(super::popup_text(KtStatus::Ok, "x").0.starts_with("Encryption was auto-verified"));
        assert_eq!(super::support_url(None), "https://support.signal.org/hc/en-us/requests/new?desktop");
        assert_eq!(
            super::support_url(Some("https://debuglogs.org/a/b.gz")),
            "https://support.signal.org/hc/en-us/requests/new?desktop&kt=https%3A%2F%2Fdebuglogs.org%2Fa%2Fb.gz"
        );
    }

    #[test]
    fn twelve_blocks_of_five() {
        let digits: String = (0..60).map(|i| char::from(b'0' + (i % 10) as u8)).collect();
        let blocks = super::blocks(&digits);
        assert_eq!(blocks.len(), 12);
        assert_eq!(blocks[0], "01234");
        assert_eq!(blocks[11], "56789");
    }
}
