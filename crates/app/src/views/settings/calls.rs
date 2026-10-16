//! Desktop's Calls page: the call devices.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, div};
use signal_backend::{Device, DeviceKind};

use super::{Choice, Settings, list, select};
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let devices = match state.read(cx).call_devices.clone() {
        Some(current) => {
            let row = |id: &'static str, label: &'static str, kind: DeviceKind, devices: Vec<Device>, chosen: Option<String>| {
                // Desktop shows the first device when none was chosen.
                let chosen = chosen.filter(|c| devices.iter().any(|d| &d.id == c)).or_else(|| devices.first().map(|d| d.id.clone()));
                let empty = devices.is_empty();
                let mut choices: Vec<Choice> = devices
                    .into_iter()
                    .map(|device| {
                        let state = state.clone();
                        let id = device.id.clone();
                        Choice::new(device.name, chosen.as_ref() == Some(&device.id), move |_, cx| {
                            state.read(cx).send(signal_backend::Command::SetCallDevice { kind, id: id.clone() })
                        })
                    })
                    .collect();
                if empty {
                    choices.push(Choice::new("No devices available", true, |_, _| {}));
                }
                select(id, label, None, choices, empty, palette)
            };
            vec![
                row("call-camera", "Video", DeviceKind::Camera, current.devices.cameras, current.camera),
                row("call-microphone", "Microphone", DeviceKind::Microphone, current.devices.microphones, current.microphone),
                row("call-speaker", "Speakers", DeviceKind::Speaker, current.devices.speakers, current.speaker),
            ]
        }
        None => vec![div().text_sm().text_color(palette.text_secondary).child("Loading devices…").into_any_element()],
    };
    div().flex().flex_col().gap_5().child(list(Some("Devices"), None, devices, palette)).into_any_element()
}
