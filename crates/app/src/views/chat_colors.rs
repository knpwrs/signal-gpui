//! Desktop's `ChatColorPicker` (`ts/components/ChatColorPicker.dom.tsx`),
//! for the default color (Settings, Appearance) and for one chat (its
//! settings), with `SampleMessageBubbles` and the `CustomColorEditor`.
//! Desktop sets a gradient's angle by dragging two knobs on a dial
//! (`GradientDial`); here the knobs are buttons choosing which end the
//! sliders change, and an angle slider turns it.

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Context, Entity, Hsla, SharedString, Subscription, TestSupportExt as _, Window, div, px};
use signal_store::{CHAT_COLOR_PRESETS, ChatColor, ChatColorEdit, ConversationId, CustomColor, HueSaturation};

use crate::chat_color::{BubbleFill, custom, fill, hsl};
use crate::state::AppState;
use crate::theme::Palette;

const SWATCH: f32 = 40.;

/// A new custom color's id: a random UUID, as Desktop's `getGuid`.
fn new_id() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// A bubble as `SampleMessageBubbles` draws it.
fn sample(text: &'static str, ours: Option<BubbleFill>, palette: Palette) -> AnyElement {
    let bubble = div().max_w(px(260.)).px_3().py_2().rounded_xl().text_sm().child(text);
    let bubble = match ours {
        Some(fill) => bubble.bg(fill.background()).text_color(palette.bubble_out_text),
        None => bubble.bg(palette.bubble_in).text_color(palette.bubble_in_text),
    };
    div().flex().when(ours.is_some(), |d| d.justify_end()).child(bubble).into_any_element()
}

/// Desktop's sample bubbles: theirs, then ours in `fill`; with
/// `another`, ours first too (the custom color editor's).
fn samples(fill: BubbleFill, another: bool, palette: Palette) -> AnyElement {
    let first = if another { Some(fill) } else { None };
    div()
        .debug_selector(|| "chat-color-samples".into())
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(palette.panel_border)
        .child(sample("Here's a preview of the chat color.", first, palette))
        .when(another, |d| d.child(sample("Another bubble.", None, palette)))
        .child(sample("The color is visible to only you.", Some(fill), palette))
        .into_any_element()
}

fn swatch(id: SharedString, fill: BubbleFill, selected: bool, palette: Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .size(px(SWATCH + 6.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_2()
        .border_color(if selected { palette.text } else { gpui_kit::transparent_black() })
        .cursor_pointer()
        .child(div().size(px(SWATCH)).rounded_full().bg(fill.background()))
}

/// The picker for `chat`, or for the default (`None`).
pub fn render(state: &Entity<AppState>, chat: Option<ConversationId>, palette: Palette, cx: &App) -> AnyElement {
    let colors = state.read(cx).chat_colors.clone();
    let selected = match &chat {
        Some(chat) => colors.of(chat).clone(),
        None => colors.default.clone(),
    };
    let choose = |color: ChatColor, chat: Option<ConversationId>, state: Entity<AppState>| {
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            let edit = match &chat {
                Some(chat) => ChatColorEdit::Chat(chat.clone(), Some(color.clone())),
                None => ChatColorEdit::Default(color.clone()),
            };
            state.update(cx, |s, cx| s.edit_chat_colors(edit, cx));
            window.refresh();
        }
    };
    let mut swatches: Vec<AnyElement> = CHAT_COLOR_PRESETS
        .iter()
        .map(|name| {
            let color = ChatColor::preset(name);
            let chosen = selected == color;
            swatch(SharedString::from(format!("chat-color-{name}")), fill(&color), chosen, palette)
                .on_click(choose(color, chat.clone(), state.clone()))
                .test_support()
                .into_any_element()
        })
        .collect();
    for (ix, (id, value)) in colors.custom.colors.iter().enumerate() {
        let color = ChatColor::Custom { id: id.clone(), value: *value };
        let chosen = selected.custom_id() == Some(id.as_str());
        let (menu_state, menu_chat, menu_id, menu_value, used) = (state.clone(), chat.clone(), id.clone(), *value, colors.chats_with(id));
        swatches.push(
            swatch(SharedString::from(format!("chat-color-custom-{ix}")), custom(value), chosen, palette)
                .on_click(choose(color, chat.clone(), state.clone()))
                .test_support()
                // Desktop's menu on a custom color: edit, duplicate, delete.
                .context_menu(move |menu, _, _| {
                    let (edit_state, edit_chat, edit_id) = (menu_state.clone(), menu_chat.clone(), menu_id.clone());
                    let (dupe_state, dupe_chat) = (menu_state.clone(), menu_chat.clone());
                    let (delete_state, delete_id) = (menu_state.clone(), menu_id.clone());
                    menu.item(PopupMenuItem::new("Edit color").on_click(move |_, window, cx| {
                        open_editor(edit_state.clone(), edit_chat.clone(), Some((edit_id.clone(), menu_value)), window, cx);
                    }))
                    .item(PopupMenuItem::new("Duplicate").on_click(move |_, _, cx| {
                        let edit = ChatColorEdit::AddCustom { id: new_id(), color: menu_value, chat: dupe_chat.clone() };
                        dupe_state.update(cx, |s, cx| s.edit_chat_colors(edit, cx));
                    }))
                    .item(PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                        delete_custom(delete_state.clone(), delete_id.clone(), used, window, cx)
                    }))
                })
                .into_any_element(),
        );
    }
    let (add_state, add_chat) = (state.clone(), chat.clone());
    swatches.push(
        div()
            .id("chat-color-add")
            .size(px(SWATCH + 6.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(palette.panel_border)
            .cursor_pointer()
            .text_lg()
            .child("+")
            .on_click(move |_, window, cx| {
                open_editor(add_state.clone(), add_chat.clone(), None, window, cx);
            })
            .test_support()
            .into_any_element(),
    );
    let reset_state = state.clone();
    let reset_chat = chat.clone().map(|chat| {
        let state = state.clone();
        Button::new("chat-color-reset").label("Reset chat color").ghost().small().on_click(move |_, window, cx| {
            state.update(cx, |s, cx| s.edit_chat_colors(ChatColorEdit::Chat(chat.clone(), None), cx));
            window.refresh();
        })
    });
    let global = chat.is_none();
    div()
        .debug_selector(|| "chat-color-picker".into())
        .flex()
        .flex_col()
        .gap_4()
        .child(samples(fill(&selected), false, palette))
        .child(div().flex().flex_wrap().gap_2().children(swatches))
        .child(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .children(reset_chat)
                .child(Button::new("chat-color-reset-all").label("Reset all chat colors").ghost().small().on_click(move |_, window, cx| {
                    confirm_reset(reset_state.clone(), global, window, cx)
                })),
        )
        .into_any_element()
}

/// Desktop's confirmations: for the default, "Reset default colors" (the
/// default only) or "Reset all chat colors" (every chat too); for a chat,
/// "Reset" (every chat).
fn confirm_reset(state: Entity<AppState>, global: bool, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, _| {
        let reset = |id: &'static str, label: &'static str, edits: Vec<ChatColorEdit>, state: Entity<AppState>| {
            Button::new(id).label(label).primary().on_click(move |_, window, cx| {
                for edit in &edits {
                    state.update(cx, |s, cx| s.edit_chat_colors(edit.clone(), cx));
                }
                window.close_dialog(cx);
            })
        };
        let footer = DialogFooter::new().child(Button::new("chat-color-reset-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_dialog(cx)));
        let footer = if global {
            footer
                .child(reset("chat-color-reset-default", "Reset default colors", vec![ChatColorEdit::ResetDefault], state.clone()))
                .child(reset("chat-color-reset-everything", "Reset all chat colors", vec![ChatColorEdit::ResetDefault, ChatColorEdit::ResetChats], state.clone()))
        } else {
            footer.child(reset("chat-color-reset-confirm", "Reset", vec![ChatColorEdit::ResetChats], state.clone()))
        };
        dialog
            .title(if global { "Reset Chat Colors" } else { "Reset all chat colors" })
            .w(px(420.))
            .child(div().text_sm().child("Would you like to override all chat colors?"))
            .footer(footer)
    });
}

/// Desktop deletes a color no chat uses at once, and asks first when some do.
fn delete_custom(state: Entity<AppState>, id: String, used: usize, window: &mut Window, cx: &mut App) {
    if used == 0 {
        state.update(cx, |s, cx| s.edit_chat_colors(ChatColorEdit::RemoveCustom { id }, cx));
        return;
    }
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, id) = (state.clone(), id.clone());
        let chats = if used == 1 { "1 chat".to_owned() } else { format!("{used} chats") };
        alert
            .title("Delete color")
            .description(format!("This custom color is used in {chats}. Do you want to delete it for all chats?"))
            .confirm()
            .ok_text("Delete")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
                state.update(cx, |s, cx| s.edit_chat_colors(ChatColorEdit::RemoveCustom { id: id.clone() }, cx));
                true
            })
    });
}

/// Which end of a gradient the sliders change (Desktop's `KnobType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    Start,
    End,
}

/// Desktop's `CustomColorEditor`.
pub struct ColorEditor {
    pub color: CustomColor,
    pub knob: Knob,
    pub hue: Entity<SliderState>,
    pub saturation: Entity<SliderState>,
    pub angle: Entity<SliderState>,
    _subscriptions: Vec<Subscription>,
}

const ULTRAMARINE_ISH: HueSaturation = CustomColor::ULTRAMARINE_ISH.start;

impl ColorEditor {
    fn new(color: CustomColor, cx: &mut Context<Self>) -> Self {
        let hue = cx.new(|_| SliderState::new().min(0.).max(360.).step(1.).default_value(color.start.hue));
        let saturation = cx.new(|_| SliderState::new().min(0.).max(100.).step(1.).default_value(color.start.saturation));
        // The angle the bubbles get: a gradient without one draws at 0
        // (`chat_color::custom`, as Desktop's `getCustomColorStyle`); a
        // gradient started here gets 180 (`set_gradient`).
        let shown = color.deg.unwrap_or(if color.end.is_some() { 0. } else { 180. });
        let angle = cx.new(|_| SliderState::new().min(0.).max(360.).step(1.).default_value(shown));
        let subscriptions = vec![
            cx.subscribe(&hue, |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = event;
                this.set_end(|end| HueSaturation { hue: v.start(), lightness: None, ..end });
                cx.notify();
            }),
            cx.subscribe(&saturation, |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = event;
                this.set_end(|end| HueSaturation { saturation: v.start(), ..end });
                cx.notify();
            }),
            cx.subscribe(&angle, |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = event;
                this.color.deg = Some(v.start());
                cx.notify();
            }),
        ];
        Self { color, knob: Knob::Start, hue, saturation, angle, _subscriptions: subscriptions }
    }

    fn set_end(&mut self, change: impl FnOnce(HueSaturation) -> HueSaturation) {
        match self.knob {
            Knob::Start => self.color.start = change(self.color.start),
            Knob::End => self.color.end = Some(change(self.color.end.unwrap_or(ULTRAMARINE_ISH))),
        }
    }

    /// Desktop's tabs: a gradient starts its end at ultramarine-ish; solid
    /// drops the end.
    pub fn set_gradient(&mut self, gradient: bool, window: &mut Window, cx: &mut Context<Self>) {
        match (gradient, self.color.end) {
            (true, None) => {
                self.color.end = Some(ULTRAMARINE_ISH);
                // The angle the slider shows (Desktop's `GradientDial`
                // defaults to 180), kept so the bubbles draw it too.
                if self.color.deg.is_none() {
                    self.color.deg = Some(180.);
                    self.angle.update(cx, |s, cx| s.set_value(180., window, cx));
                }
            }
            (false, Some(_)) => {
                self.color.end = None;
                self.select(Knob::Start, window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Chooses the end the sliders change, and shows its values.
    pub fn select(&mut self, knob: Knob, window: &mut Window, cx: &mut Context<Self>) {
        self.knob = knob;
        let end = match knob {
            Knob::Start => self.color.start,
            Knob::End => self.color.end.unwrap_or(ULTRAMARINE_ISH),
        };
        self.hue.update(cx, |s, cx| s.set_value(end.hue, window, cx));
        self.saturation.update(cx, |s, cx| s.set_value(end.saturation, window, cx));
        cx.notify();
    }
}

impl Render for ColorEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let gradient = self.color.end.is_some();
        let tab = |id: &'static str, label: &'static str, to_gradient: bool, cx: &mut Context<Self>| {
            Button::new(id)
                .label(label)
                .small()
                .map(|b| if gradient == to_gradient { b.primary() } else { b.ghost() })
                .on_click(cx.listener(move |this, _, window, cx| this.set_gradient(to_gradient, window, cx)))
        };
        let knob = |id: &'static str, label: &'static str, which: Knob, color: Hsla, this: &Self, cx: &mut Context<Self>| {
            Button::new(id)
                .small()
                .map(|b| if this.knob == which { b.primary() } else { b.outline() })
                .child(div().flex().items_center().gap_1().child(div().size(px(12.)).rounded_full().bg(color)).child(label))
                .on_click(cx.listener(move |this, _, window, cx| this.select(which, window, cx)))
        };
        let current = match self.knob {
            Knob::Start => self.color.start,
            Knob::End => self.color.end.unwrap_or(ULTRAMARINE_ISH),
        };
        let slider_row = |label: &'static str, slider: &Entity<SliderState>, swatch: Hsla| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(div().flex().items_center().gap_2().child(div().flex_1().child(Slider::new(slider).horizontal())).child(div().size(px(16.)).rounded_full().bg(swatch)))
        };
        div()
            .debug_selector(|| "custom-color-editor".into())
            .flex()
            .flex_col()
            .gap_3()
            .child(div().flex().gap_2().child(tab("custom-color-solid", "Solid", false, cx)).child(tab("custom-color-gradient", "Gradient", true, cx)))
            .child(samples(custom(&self.color), true, palette))
            .when(gradient, |d| {
                let (start, end) = (hsl(self.color.start), hsl(self.color.end.unwrap_or(ULTRAMARINE_ISH)));
                d.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(knob("custom-color-start", "Start", Knob::Start, start, self, cx))
                        .child(knob("custom-color-end", "End", Knob::End, end, self, cx)),
                )
                .child(slider_row("Angle", &self.angle, palette.text_secondary))
            })
            .child(slider_row("Hue", &self.hue, hsl(HueSaturation { saturation: 100., lightness: None, ..current })))
            .child(slider_row("Saturation", &self.saturation, hsl(current)))
    }
}

/// Desktop's custom color editor: a new color for `chat` (or the default),
/// or `existing` edited (and chosen for `chat`, or the default, as
/// Desktop's `onSave` chooses it).
pub fn open_editor(
    state: Entity<AppState>,
    chat: Option<ConversationId>,
    existing: Option<(String, CustomColor)>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ColorEditor> {
    let start = existing.as_ref().map_or(CustomColor::ULTRAMARINE_ISH, |(_, c)| *c);
    let editor = cx.new(|cx| ColorEditor::new(start, cx));
    let shown = editor.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let (editor_save, state, chat, existing) = (editor.clone(), state.clone(), chat.clone(), existing.clone());
        dialog.title("Custom Color").w(px(440.)).child(editor.clone()).footer(
            DialogFooter::new()
                .child(Button::new("custom-color-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("custom-color-save").label("Save").primary().on_click(move |_, window, cx| {
                    let color = editor_save.read(cx).color;
                    let edits = save_edits(color, chat.clone(), existing.as_ref().map(|(id, _)| id.clone()));
                    for edit in edits {
                        state.update(cx, |s, cx| s.edit_chat_colors(edit, cx));
                    }
                    window.close_dialog(cx);
                })),
        )
    });
    shown
}

/// What Save does (Desktop's `CustomColorEditorWrapper` `onSave`): an
/// edited color changes and is chosen; a new one is added and chosen.
pub fn save_edits(color: CustomColor, chat: Option<ConversationId>, existing: Option<String>) -> Vec<ChatColorEdit> {
    match existing {
        Some(id) => {
            let chosen = ChatColor::Custom { id: id.clone(), value: color };
            vec![
                ChatColorEdit::EditCustom { id, color },
                match chat {
                    Some(chat) => ChatColorEdit::Chat(chat, Some(chosen)),
                    None => ChatColorEdit::Default(chosen),
                },
            ]
        }
        None => vec![ChatColorEdit::AddCustom { id: new_id(), color, chat }],
    }
}

/// Desktop's chat color panel for one chat (from its settings).
pub fn open_for_chat(state: Entity<AppState>, chat: ConversationId, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        dialog.title("Chat Color").w(px(460.)).child(render(&state, Some(chat.clone()), palette, cx))
    });
}

/// Desktop's chat color swatch beside "Chat color" rows.
pub fn swatch_for(color: &ChatColor) -> AnyElement {
    div().size(px(20.)).rounded_full().bg(fill(color).background()).into_any_element()
}
