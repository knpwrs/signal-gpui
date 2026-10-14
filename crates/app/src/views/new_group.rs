//! Desktop's "New group": pick members, name it, create.

use std::collections::BTreeSet;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, SharedString, Window, div, px};
use signal_backend::Command;
use signal_store::DirectoryEntry;

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::person_avatar;

struct Picking {
    people: Vec<DirectoryEntry>,
    chosen: BTreeSet<String>,
}

pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Group name (required)"));
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search name or number"));
    let picking = cx.new(|_| Picking { people: Vec::new(), chosen: BTreeSet::new() });
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let query = backend.spawn(async move { db.people().await });
    let loaded = picking.clone();
    cx.spawn(async move |cx| {
        let people = query.await.unwrap_or_default();
        loaded.update(cx, |p, cx| {
            p.people = people;
            cx.notify();
        });
    })
    .detach();
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let needle = search.read(cx).value().trim().to_lowercase();
        let chosen = picking.read(cx).chosen.clone();
        let people: Vec<DirectoryEntry> = picking
            .read(cx)
            .people
            .iter()
            .filter(|p| needle.is_empty() || p.title.to_lowercase().contains(&needle) || p.phone.as_deref().is_some_and(|ph| ph.contains(&needle)))
            .cloned()
            .collect();
        let rows = people.into_iter().map(|person| {
            let sid = person.id.as_direct().unwrap_or_default().to_owned();
            let on = chosen.contains(&sid);
            let picking = picking.clone();
            let toggle = sid.clone();
            div()
                .id(SharedString::from(format!("pick-{sid}")))
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(move |_, window, cx| {
                    picking.update(cx, |p, _| {
                        if !p.chosen.remove(&toggle) {
                            p.chosen.insert(toggle.clone());
                        }
                    });
                    window.refresh();
                })
                .child(person_avatar(state.read(cx).photo_of(&sid), &person.title, color_for(&person.id.0), px(32.)))
                .child(div().flex_1().truncate().child(person.title.clone()))
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .border_2()
                        .border_color(if on { palette.accent } else { palette.text_secondary })
                        .when(on, |d| d.bg(palette.accent)),
                )
        });
        let (create_state, create_name, create_picking) = (state.clone(), name.clone(), picking.clone());
        let ready = !name.read(cx).value().trim().is_empty() && !chosen.is_empty();
        dialog
            .title("New group")
            .w(px(440.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&name))
                    .child(Input::new(&search))
                    .child(div().text_xs().text_color(palette.text_secondary).child(format!("{} chosen", chosen.len())))
                    .child(div().id("new-group-people").h(px(320.)).overflow_y_scroll().children(rows)),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("new-group-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("new-group-create").label("Create").primary().disabled(!ready).on_click(
                        move |_, window, cx| {
                            let title = create_name.read(cx).value().trim().to_string();
                            let members: Vec<String> = create_picking.read(cx).chosen.iter().cloned().collect();
                            create_state.read(cx).send(Command::CreateGroup { title, members });
                            window.close_all_dialogs(cx);
                        },
                    )),
            )
    });
}
