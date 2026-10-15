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
                    .child(section("Notifications", palette))
                    .child(notifications(&state, cx))
                    .child(section("Calls", palette))
                    .child(call_devices(&state, cx))
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
