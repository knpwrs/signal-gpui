//! Desktop's "Forward To" dialog: pick up to five chats, then Continue.

use std::collections::BTreeSet;

use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, SharedString, Window, div, px};
use signal_backend::Command;
use signal_store::{ConversationId, ConversationKind};

use crate::state::AppState;
use crate::theme::Palette;
use crate::views::common::conversation_avatar;

/// Desktop's `MAX_FORWARD`.
pub const MAX_CHATS: usize = 5;
/// Desktop forwards at most this many messages at once.
pub const MAX_MESSAGES: usize = 30;

struct Picked(BTreeSet<ConversationId>);

pub fn open(state: Entity<AppState>, message_ids: Vec<i64>, window: &mut Window, cx: &mut App) {
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
    let picked = cx.new(|_| Picked(BTreeSet::new()));
    window.open_dialog(cx, move |dialog, window, cx| {
        let palette = Palette::get(cx);
        let query = search.read(cx).value().trim().to_lowercase();
        let chosen = picked.read(cx).0.clone();
        let full = chosen.len() >= MAX_CHATS;
        let chats: Vec<_> = state
            .read(cx)
            .conversations
            .iter()
            .filter(|c| !c.blocked)
            .filter(|c| query.is_empty() || c.title.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let rows = chats.into_iter().map(|chat| {
            let on = chosen.contains(&chat.id);
            let id = chat.id.clone();
            let picked = picked.clone();
            let title = if chat.kind == ConversationKind::NoteToSelf { "Note to Self".to_owned() } else { chat.title.clone() };
            div()
                .id(SharedString::from(format!("forward-{}", chat.id.0)))
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .when(on || !full, |d| d.cursor_pointer().hover(|d| d.bg(palette.row_hover)))
                .when(!on && full, |d| d.opacity(0.5))
                .on_click(move |_, window, cx| {
                    picked.update(cx, |p, _| {
                        if !p.0.remove(&id) && p.0.len() < MAX_CHATS {
                            p.0.insert(id.clone());
                        }
                    });
                    window.refresh();
                })
                .child(conversation_avatar(&chat, px(32.)))
                .child(div().flex_1().truncate().child(title))
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .border_2()
                        .border_color(if on { palette.accent } else { palette.text_secondary })
                        .when(on, |d| d.bg(palette.accent)),
                )
        });
        let (state, picked_now, ids) = (state.clone(), picked.clone(), message_ids.clone());
        let _ = window;
        dialog
            .title("Forward To")
            .w(px(420.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&search))
                    .child(div().id("forward-chats").h(px(360.)).overflow_y_scroll().children(rows)),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("forward-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(
                        Button::new("forward-continue")
                            .label("Continue")
                            .primary()
                            .disabled(chosen.is_empty())
                            .on_click(move |_, window, cx| {
                                let conversations: Vec<ConversationId> = picked_now.read(cx).0.iter().cloned().collect();
                                state.read(cx).send(Command::Forward { message_ids: ids.clone(), conversations });
                                window.close_all_dialogs(cx);
                            }),
                    ),
            )
    });
}
