//! Desktop's "All media": a chat's photos and GIFs, files, and links.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, ObjectFit, SharedString, Window, div, img, px};
use signal_store::{ConversationId, Message};

use crate::state::{AppEvent, AppState};
use crate::theme::Palette;
use crate::views::image_viewer::ViewedImage;

/// Desktop loads media in pages; a few hundred covers most chats.
const LIMIT: u32 = 500;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Media,
    Documents,
    Links,
}

struct AllMedia {
    messages: Vec<Message>,
    tab: Tab,
}

pub fn open(state: Entity<AppState>, conversation: ConversationId, window: &mut Window, cx: &mut App) {
    let model = cx.new(|_| AllMedia { messages: Vec::new(), tab: Tab::Media });
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let query = backend.spawn(async move { db.media_messages(&conversation, LIMIT).await });
    let loaded = model.clone();
    cx.spawn(async move |cx| {
        let messages = query.await.unwrap_or_default();
        loaded.update(cx, |model, cx| {
            model.messages = messages;
            cx.notify();
        });
    })
    .detach();
    window.open_dialog(cx, move |dialog, window, cx| {
        let palette = Palette::get(cx);
        let tab = model.read(cx).tab;
        let messages = model.read(cx).messages.clone();
        let tab_button = |id: &'static str, label: &'static str, which: Tab| {
            let model = model.clone();
            Button::new(id)
                .label(label)
                .when(tab == which, |b| b.primary())
                .when(tab != which, |b| b.ghost())
                .on_click(move |_, window, cx| {
                    model.update(cx, |m, _| m.tab = which);
                    window.refresh();
                })
        };
        let body = match tab {
            Tab::Media => media_grid(&messages, &state, palette).into_any_element(),
            Tab::Documents => documents(&messages, palette).into_any_element(),
            Tab::Links => links(&messages, palette).into_any_element(),
        };
        let _ = window;
        dialog.title("All media").w(px(560.)).child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(tab_button("media-tab", "Media", Tab::Media))
                        .child(tab_button("documents-tab", "Documents", Tab::Documents))
                        .child(tab_button("links-tab", "Links", Tab::Links)),
                )
                .child(div().id("all-media-body").h(px(420.)).overflow_y_scroll().child(body)),
        )
    });
}

fn empty(text: &'static str, palette: Palette) -> gpui_kit::AnyElement {
    div().p_6().text_center().text_color(palette.text_secondary).child(text).into_any_element()
}

/// Downloaded photos. (GIFs are videos, which need decoding to show.)
fn media_grid(messages: &[Message], state: &Entity<AppState>, palette: Palette) -> gpui_kit::AnyElement {
    let tiles: Vec<_> = messages
        .iter()
        .flat_map(|m| m.attachments.iter())
        .filter(|a| !a.sticker && !a.gif && a.content_type.starts_with("image/"))
        .filter_map(|a| Some((a.local_path.clone()?, a.clone())))
        .collect();
    if tiles.is_empty() {
        return empty("No media", palette);
    }
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(tiles.into_iter().enumerate().map(|(ix, (path, meta))| {
            let state = state.clone();
            let viewed = ViewedImage {
                path: path.clone().into(),
                size: meta.width.zip(meta.height),
                file_name: meta.file_name.clone(),
            };
            div()
                .id(("media-tile", ix))
                .size(px(128.))
                .rounded_md()
                .overflow_hidden()
                .cursor_pointer()
                .on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    let viewed = viewed.clone();
                    state.update(cx, |_, cx| cx.emit(AppEvent::ViewImage(viewed)));
                })
                .child(img(std::path::PathBuf::from(path)).size_full().object_fit(ObjectFit::Cover))
        }))
        .into_any_element()
}

/// Files that aren't photos, GIFs or stickers.
fn documents(messages: &[Message], palette: Palette) -> gpui_kit::AnyElement {
    let files: Vec<_> = messages
        .iter()
        .flat_map(|m| m.attachments.iter().map(move |a| (m.sent_at, a)))
        .filter(|(_, a)| !a.sticker && !a.gif && !a.content_type.starts_with("image/"))
        .collect();
    if files.is_empty() {
        return empty("No documents", palette);
    }
    div()
        .flex()
        .flex_col()
        .children(files.into_iter().map(|(at, a)| {
            div()
                .flex()
                .justify_between()
                .py_1p5()
                .child(div().truncate().child(a.file_name.clone().unwrap_or_else(|| "Attachment".into())))
                .child(div().text_sm().text_color(palette.text_secondary).child(crate::views::common::short_timestamp(at)))
        }))
        .into_any_element()
}

fn links(messages: &[Message], palette: Palette) -> gpui_kit::AnyElement {
    let previews: Vec<_> = messages.iter().filter_map(|m| m.link_preview.clone()).collect();
    if previews.is_empty() {
        return empty("No links", palette);
    }
    div()
        .flex()
        .flex_col()
        .children(previews.into_iter().enumerate().map(|(ix, preview)| {
            let url = preview.url.clone();
            div()
                .id(("link-row", ix))
                .py_1p5()
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(move |_, _, cx| cx.open_url(&url))
                .child(div().truncate().child(SharedString::from(preview.title.clone().unwrap_or_else(|| preview.url.clone()))))
                .child(div().text_sm().truncate().text_color(palette.text_secondary).child(preview.url.clone()))
        }))
        .into_any_element()
}
