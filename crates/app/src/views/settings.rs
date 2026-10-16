//! Settings dialog (⌘,): account details and unlinking this device.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, FontWeight, SharedString, Window, div, px};

use crate::state::AppState;
use crate::theme::Palette;

pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    state.read(cx).send(signal_backend::Command::LoadCallDevices);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let account = state.read(cx).account.clone();
        let phone = account
            .as_ref()
            .and_then(|a| a.phone.clone())
            .unwrap_or_else(|| "Unknown".into());
        let aci = account.map(|a| a.aci).unwrap_or_default();
        let state = state.clone();
        let log_state = state.clone();
        dialog
            .title("Settings")
            .w(px(440.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(section("Account", palette))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(field("Phone number", phone, palette))
                            .child(field("Account ID", aci, palette)),
                    )
                    .child(section("Privacy", palette))
                    .child(privacy(&state, cx))
                    .when(signal_backend::key_transparency_on(), |d| d.child(key_transparency(&state, cx)))
                    .child(section("Notifications", palette))
                    .child(notifications(&state, cx))
                    .child(section("Calls", palette))
                    .child(call_devices(&state, cx))
                    .child(section("Stories", palette))
                    .child(stories(&state, cx))
                    .child(section("Images", palette))
                    .child(image_optimization(&state, cx))
                    .child(section("Linked device", palette))
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.text_secondary)
                            .child(
                                "This computer is linked to your phone. Unlinking signs it out, \
                                 removes its keys, and deletes the messages and media stored on \
                                 this computer.",
                            ),
                    )
                    .child(
                        div().child(
                            Button::new("unlink")
                                .danger()
                                .label("Unlink this device…")
                                .on_click(move |_, window, cx| confirm_unlink(state.clone(), window, cx)),
                        ),
                    )
                    .child(section("Help", palette))
                    .child(
                        div().child(
                            Button::new("debug-log")
                                .label("Debug log…")
                                .on_click(move |_, window, cx| crate::views::debug_log::show(log_state.clone(), window, cx)),
                        ),
                    )
                    .child(section("About", palette))
                    .child(
                        div().child(
                            Button::new("about")
                                .label("About & licenses…")
                                .on_click(|_, window, cx| crate::views::about::show(window, cx)),
                        ),
                    ),
            )
    });
}

fn confirm_unlink(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .title("Unlink this device?")
            .description("Messages and media on this computer will be deleted. You'll need to scan a new QR code from your phone to use Signal here again.")
            .confirm()
            .ok_text("Unlink")
            .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.read(cx).send(signal_backend::Command::Unlink);
                window.close_all_dialogs(cx);
                true
            })
    });
}

/// Desktop's Privacy page, "Messaging": read receipts and typing
/// indicators, kept in the account record so the phone and other devices
/// follow. Off, neither is sent nor shown from others.
fn privacy(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    use signal_backend::AccountSetting;
    let palette = Palette::get(cx);
    let (read_receipts, typing) = {
        let s = state.read(cx);
        (s.read_receipts, s.typing_indicators)
    };
    let toggle = |id: &'static str, label: &'static str, setting: AccountSetting, on: bool| {
        let state = state.clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(div().text_sm().child(label))
            .child(
                Button::new(id)
                    .label(if on { "On" } else { "Off" })
                    .small()
                    .when(on, |b| b.primary())
                    .when(!on, |b| b.outline())
                    .on_click(move |_, window, cx| {
                        state.update(cx, |s, cx| s.set_account_setting(setting, !on, cx));
                        window.refresh();
                    }),
            )
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(toggle("read-receipts", "Read receipts", AccountSetting::ReadReceipts, read_receipts))
        .child(toggle("typing-indicators", "Typing indicators", AccountSetting::TypingIndicators, typing))
        .child(div().text_xs().text_color(palette.text_secondary).child(
            "See and share when messages are being read and typed. If disabled, you won't see read receipts or typing indicators from others.",
        ))
}

/// Desktop's notification content choice, plus off.
fn notifications(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    use crate::notifications::NotificationSetting;
    let current = state.read(cx).notification_setting;
    let choice = |id: &'static str, label: &'static str, setting: NotificationSetting| {
        let state = state.clone();
        Button::new(id)
            .label(label)
            .small()
            .when(current == setting, |b| b.primary())
            .when(current != setting, |b| b.outline())
            .on_click(move |_, window, cx| {
                state.update(cx, |state, cx| state.set_notification_setting(setting, cx));
                window.refresh();
            })
    };
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .child(choice("notify-message", "Name, content, and actions", NotificationSetting::NameAndMessage))
        .child(choice("notify-name", "Name only", NotificationSetting::NameOnly))
        .child(choice("notify-none", "No name or content", NotificationSetting::NoNameOrMessage))
        .child(choice("notify-off", "Off", NotificationSetting::Off))
}

/// Desktop's "Turn off stories" confirmation (`Preferences`). It deletes
/// the stories we sent, as its text says; Desktop's own code
/// (`ts/state/smart/Preferences.preload.tsx`) calls `deleteAllMyStories`
/// on turning stories back on instead, which looks like a slip.
pub fn confirm_stories_off(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .description("You will no longer be able to share or view stories. Story updates you have recently shared will also be deleted.")
            .confirm()
            .ok_text("Turn off")
            .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.update(cx, |s, cx| s.set_stories_disabled(true, cx));
                window.close_all_dialogs(cx);
                true
            })
    });
}

/// Desktop's "Share & View Stories" (Privacy) and its story view receipts
/// (Story privacy), both kept in the account record.
fn stories(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let palette = Palette::get(cx);
    let (disabled, receipts) = {
        let s = state.read(cx);
        (s.stories_disabled, s.story_view_receipts_on())
    };
    let (toggle_state, receipts_state) = (state.clone(), state.clone());
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_sm().text_color(palette.text_secondary).child(
            "If you opt out of stories you will no longer be able to share or view stories.",
        ))
        .child(
            div().child(
                Button::new("stories-toggle")
                    .label(if disabled { "Turn on stories" } else { "Turn off stories" })
                    .small()
                    .outline()
                    .on_click(move |_, window, cx| {
                        if disabled {
                            toggle_state.update(cx, |s, cx| s.set_stories_disabled(false, cx));
                            window.refresh();
                        } else {
                            confirm_stories_off(toggle_state.clone(), window, cx);
                        }
                    }),
            ),
        )
        .when(!disabled, |d| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(palette.text_secondary)
                            .child("View receipts: see and share when stories are viewed. If disabled, you won't see when others view your story."),
                    )
                    .child(
                        Button::new("story-view-receipts")
                            .label(if receipts { "On" } else { "Off" })
                            .small()
                            .when(receipts, |b| b.primary())
                            .when(!receipts, |b| b.outline())
                            .on_click(move |_, window, cx| {
                                receipts_state.update(cx, |s, cx| s.set_story_view_receipts(!receipts, cx));
                                window.refresh();
                            }),
                    ),
            )
        })
}

/// Desktop's "Automatic key verification" (Privacy), kept in the account
/// record.
fn key_transparency(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let palette = Palette::get(cx);
    let on = !state.read(cx).key_transparency_disabled;
    let state = state.clone();
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(div().text_sm().child("Automatic key verification"))
                .child(
                    div()
                        .text_sm()
                        .text_color(palette.text_secondary)
                        .child("When enabled, Signal will attempt to automatically verify the encryption of 1:1 chats."),
                )
                .child(
                    div().child(
                        Button::new("key-transparency-learn-more")
                            .link()
                            .small()
                            .label("Learn More")
                            .on_click(|_, _, cx| cx.open_url("https://support.signal.org/hc/articles/10223569377562")),
                    ),
                ),
        )
        .child(
            Button::new("key-transparency-toggle")
                .label(if on { "On" } else { "Off" })
                .small()
                .when(on, |b| b.primary())
                .when(!on, |b| b.outline())
                .on_click(move |_, window, cx| {
                    state.update(cx, |s, cx| s.set_key_transparency_disabled(on, cx));
                    window.refresh();
                }),
        )
}

/// Desktop's call device choices: microphone, speaker and camera.
fn call_devices(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
    use signal_backend::{Device, DeviceKind};
    let palette = Palette::get(cx);
    let Some(current) = state.read(cx).call_devices.clone() else {
        return div().text_sm().text_color(palette.text_secondary).child("Loading devices…").into_any_element();
    };
    let row = |id: &'static str, label: &'static str, kind: DeviceKind, devices: Vec<Device>, chosen: Option<String>| {
        let name = chosen
            .as_ref()
            .and_then(|c| devices.iter().find(|d| &d.id == c))
            .map_or_else(|| "Default".to_owned(), |d| d.name.clone());
        let state = state.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(96.)).text_sm().text_color(palette.text_secondary).child(label))
            .child(Button::new(id).outline().small().label(name).dropdown_menu(move |mut menu, _, _| {
                for device in &devices {
                    let (state, device_id) = (state.clone(), device.id.clone());
                    menu = menu.item(
                        PopupMenuItem::new(device.name.clone())
                            .checked(chosen.as_ref() == Some(&device.id))
                            .on_click(move |_, _, cx| {
                                state.read(cx).send(signal_backend::Command::SetCallDevice { kind, id: device_id.clone() })
                            }),
                    );
                }
                menu
            }))
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(row("call-microphone", "Microphone", DeviceKind::Microphone, current.devices.microphones, current.microphone))
        .child(row("call-speaker", "Speaker", DeviceKind::Speaker, current.devices.speakers, current.speaker))
        .child(row("call-camera", "Camera", DeviceKind::Camera, current.devices.cameras, current.camera))
        .into_any_element()
}

/// Lossless optimization of images you send (not a Signal setting).
fn image_optimization(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    use signal_backend::media::Optimization;
    let palette = Palette::get(cx);
    let current = state.read(cx).image_optimization;
    let choice = |id: &'static str, label: &'static str, level: Optimization| {
        let state = state.clone();
        Button::new(id)
            .label(label)
            .small()
            .when(current == level, |b| b.primary())
            .when(current != level, |b| b.outline())
            .on_click(move |_, window, cx| {
                state.update(cx, |state, cx| state.set_image_optimization(level, cx));
                window.refresh();
            })
    };
    let explanation = match current {
        Optimization::Max => {
            "Images you send are made as small as possible without changing a pixel (oxipng for PNG, \
             mozjpeg for JPEG). Large images can take a few seconds."
        }
        Optimization::Fast => "Images are made smaller without changing a pixel, trading some savings for speed.",
        Optimization::Off => "Images are sent as they are, apart from removing metadata such as location.",
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .text_sm()
                .child(div().text_color(palette.text_secondary).child("Optimize images before sending"))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(choice("image-optimization-max", "Maximum", Optimization::Max))
                        .child(choice("image-optimization-fast", "Fast", Optimization::Fast))
                        .child(choice("image-optimization-off", "Off", Optimization::Off)),
                ),
        )
        .child(div().text_xs().text_color(palette.text_secondary).child(explanation))
}

fn section(title: &'static str, palette: Palette) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(palette.text_secondary)
        .child(title.to_uppercase())
}

fn field(label: &'static str, value: impl Into<SharedString>, palette: Palette) -> impl IntoElement {
    div()
        .flex()
        .justify_between()
        .gap_4()
        .text_sm()
        .child(div().flex_none().text_color(palette.text_secondary).child(label))
        .child(div().min_w_0().truncate().child(value.into()))
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowBounds, WindowOptions, px, size};
    use signal_backend::{Backend, Config};

    /// Waits (a few seconds at most) for `done`.
    fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) -> bool {
        (0..300).any(|_| {
            cx.run_until_parked();
            let ok = done(cx);
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            ok
        })
    }

    #[gpui_kit::test]
    fn privacy_toggles_turn_read_receipts_and_typing_off(cx: &mut TestAppContext) {
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
        // The demo account has both on.
        assert!(wait(cx, |cx| state.read_with(cx, |s, _| s.read_receipts && s.typing_indicators)), "demo settings didn't load");
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(800.), px(1400.)) })),
            ..Default::default()
        };
        let (window, _sidebar) = cx
            .update(|cx| {
                let state = state.clone();
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::sidebar::Sidebar::new(state, window, cx)))
            })
            .unwrap();
        cx.update_window(window, |_, window, cx| super::open(state.clone(), window, cx)).unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.click("read-receipts", cx)).unwrap();
        cx.update_window(window, |_, window, cx| window.click("typing-indicators", cx)).unwrap();
        let stored = |key: &'static str| {
            let db = backend.db().clone();
            futures::executor::block_on(backend.spawn(async move { db.setting(key).await })).ok().flatten()
        };
        assert!(
            wait(cx, |_| stored(signal_backend::SETTING_READ_RECEIPTS).as_deref() == Some("false")
                && stored(signal_backend::SETTING_TYPING_INDICATORS).as_deref() == Some("false")),
            "the settings weren't saved"
        );
        // And they stay off once the backend says they changed.
        assert!(wait(cx, |cx| state.read_with(cx, |s, _| !s.read_receipts && !s.typing_indicators)));
        // Story view receipts, unset, follow read receipts.
        assert!(!state.read_with(cx, |s, _| s.story_view_receipts_on()));
        backend.shutdown();
    }
}
