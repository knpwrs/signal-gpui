//! Desktop's "Customize reactions"
//! (`ts/components/CustomizingPreferredReactionsModal.dom.tsx`): the
//! reaction bar's six emoji; click one, pick its replacement, then Save
//! (for the whole account) or Reset to Signal's.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, Subscription, Window, div, px};
use signal_emoji::SkinTone;

use crate::state::AppState;
use crate::theme::Palette;
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};

pub(crate) struct Draft {
    original: Vec<String>,
    draft: Vec<String>,
    defaults: Vec<String>,
    /// The one being replaced (Desktop's `selectedDraftEmojiIndex`).
    selected: Option<usize>,
    picker: Entity<EmojiPicker>,
    _picked: Option<Subscription>,
}

impl Draft {
    fn changed(&self) -> bool {
        self.draft != self.original
    }

    fn is_defaults(&self) -> bool {
        self.draft == self.defaults
    }

    fn pick(&mut self, glyph: String) {
        if let Some(slot) = self.selected.take().and_then(|ix| self.draft.get_mut(ix)) {
            *slot = glyph;
        }
    }
}

pub fn open(state: Entity<AppState>, tone: Option<SkinTone>, window: &mut Window, cx: &mut App) -> Entity<Draft> {
    let original = crate::prefs::preferred_reactions(&state.read(cx).prefs.reactions, tone);
    let picker = cx.new(|cx| {
        let mut picker = EmojiPicker::new(state.clone(), window, cx);
        picker.set_media(false, window, cx);
        picker
    });
    let model = cx.new(|_| Draft {
        draft: original.clone(),
        original,
        defaults: crate::prefs::default_reactions(tone),
        selected: None,
        picker: picker.clone(),
        _picked: None,
    });
    let subscription = {
        let model = model.clone();
        window.subscribe(&picker, cx, move |_, event: &PickerEvent, window, cx| {
            model.update(cx, |m, _| match event {
                PickerEvent::Picked(glyph) => m.pick(glyph.clone()),
                _ => m.selected = None,
            });
            window.refresh();
        })
    };
    model.update(cx, |m, _| m._picked = Some(subscription));
    let shown = model.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let draft = model.read(cx);
        let (selected, changed, defaults) = (draft.selected, draft.changed(), draft.is_defaults());
        let slots = draft.draft.clone();
        let picker = draft.picker.clone();
        let (reset, save, state) = (model.clone(), model.clone(), state.clone());
        dialog
            .title("Customize reactions")
            .w(px(420.))
            .child(
                div()
                    .debug_selector(|| "customize-reactions-dialog".into())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_4()
                    .py_4()
                    .child(div().flex().gap_1().p_1().rounded_full().border_1().border_color(palette.panel_border).children(
                        slots.into_iter().enumerate().map(|(ix, glyph)| {
                            let model = model.clone();
                            div()
                                .id(("reaction-slot", ix))
                                .debug_selector(move || format!("reaction-slot-{ix}"))
                                .size(px(40.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .text_size(px(24.))
                                .cursor_pointer()
                                .when(selected == Some(ix), |d| d.bg(palette.reaction_chip_mine))
                                .hover(|d| d.bg(palette.row_hover))
                                .on_click(move |_, window, cx| {
                                    model.update(cx, |m, _| m.selected = if m.selected == Some(ix) { None } else { Some(ix) });
                                    window.refresh();
                                })
                                .child(glyph)
                        }),
                    ))
                    .when(selected.is_some(), |d| d.child(picker))
                    .child(div().text_sm().text_color(palette.text_secondary).child("Click to replace an emoji")),
            )
            .footer(
                DialogFooter::new()
                    // Desktop's Reset (here enabled once they differ from
                    // Signal's; Desktop 8.34's check reads the other way).
                    .child(Button::new("reactions-reset").label("Reset").outline().disabled(defaults).on_click(move |_, window, cx| {
                        reset.update(cx, |m, _| {
                            m.draft = m.defaults.clone();
                            m.selected = None;
                        });
                        window.refresh();
                    }))
                    .child(Button::new("reactions-save").label("Save").primary().disabled(!changed).on_click(move |_, window, cx| {
                        let reactions = save.read(cx).draft.clone();
                        state.update(cx, |s, cx| s.set_preferred_reactions(reactions, cx));
                        window.close_dialog(cx);
                    })),
            )
    });
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::test_support::{click_at, demo, press, shows, wait};

    #[gpui_kit::test]
    fn a_reaction_is_replaced_and_saved_for_the_account(cx: &mut gpui_kit::TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        let draft = cx.update(|window, cx| open(state.clone(), None, window, cx));
        wait(cx, "the dialog", &mut |cx| shows(cx, "customize-reactions-dialog"));
        click_at(cx, "reaction-slot-5");
        assert_eq!(draft.read_with(cx, |d, _| d.selected), Some(5));
        // The picker hands over its pick.
        let picker = draft.read_with(cx, |d, _| d.picker.clone());
        picker.update(cx, |_, cx| cx.emit(PickerEvent::Picked("🔥".into())));
        cx.run_until_parked();
        assert_eq!(draft.read_with(cx, |d, _| d.draft[5].clone()), "🔥");
        press(cx, "reactions-save");
        assert_eq!(state.read_with(cx, |s, _| s.prefs.reactions.clone())[5], "🔥");
        // Saved for the account (the demo keeps it as the backend does).
        wait(cx, "the setting", &mut |_| {
            let db = backend.db().clone();
            let saved = futures::executor::block_on(backend.spawn(async move { db.setting(signal_backend::SETTING_PREFERRED_REACTIONS).await }));
            saved.ok().flatten().is_some_and(|json| json.contains("🔥"))
        });
        backend.shutdown();
    }
}
