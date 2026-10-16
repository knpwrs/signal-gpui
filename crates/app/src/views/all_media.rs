//! Desktop's "All media" (`ts/components/conversation/media-gallery/`): a
//! chat's photos, audio, files and links, each with Desktop's menu (View
//! in chat, Forward, Save).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, ObjectFit, SharedString, Window, div, img, px};
use signal_backend::Command;
use signal_store::{AttachmentMeta, ConversationId, Direction, Message};

use crate::state::{AppEvent, AppState};
use crate::theme::Palette;
use crate::views::common::{format_size, short_timestamp};
use crate::views::image_viewer::ViewedImage;
use crate::views::lightbox::InChat;

/// Desktop loads media in pages; a few hundred covers most chats.
const LIMIT: u32 = 500;

/// Desktop's tabs (`MediaTabType`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tab {
    Media,
    Audio,
    Files,
    Links,
}

struct AllMedia {
    messages: Vec<Message>,
    tab: Tab,
}

/// An attachment in the gallery, with its message.
#[derive(Clone)]
struct Item {
    conversation: ConversationId,
    message_id: i64,
    index: usize,
    sent_at: i64,
    sender: String,
    attachment: AttachmentMeta,
}

/// Which tab an attachment belongs in (Desktop's `getSortedMedia` types):
/// photos and videos, audio (voice messages too), and every other file.
/// Stickers, GIFs and long texts aren't listed.
fn tab_of(attachment: &AttachmentMeta) -> Option<Tab> {
    let kind = attachment.content_type.as_str();
    if attachment.sticker || attachment.gif || kind == "text/x-signal-plain" {
        None
    } else if attachment.voice || kind.starts_with("audio/") {
        Some(Tab::Audio)
    } else if kind.starts_with("image/") || kind.starts_with("video/") {
        Some(Tab::Media)
    } else {
        Some(Tab::Files)
    }
}

/// A tab's attachments, newest first.
fn items(messages: &[Message], tab: Tab) -> Vec<Item> {
    messages
        .iter()
        // A shared contact, payment or gift isn't listed.
        .filter(|m| !m.deleted && m.view_once.is_none() && m.content.is_none())
        .flat_map(|m| {
            let sender = match m.direction {
                Direction::Outgoing => "You".to_owned(),
                Direction::Incoming => m.author_name.clone().unwrap_or_else(|| "Unknown".into()),
            };
            m.attachments.iter().enumerate().filter(move |(_, a)| tab_of(a) == Some(tab)).map(move |(index, a)| Item {
                conversation: m.conversation_id.clone(),
                message_id: m.id,
                index,
                sent_at: m.sent_at,
                sender: sender.clone(),
                attachment: a.clone(),
            })
        })
        .collect()
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
    window.open_dialog(cx, move |dialog, _, cx| {
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
            Tab::Media => media_grid(&items(&messages, Tab::Media), &state, palette),
            Tab::Audio => audio_list(&items(&messages, Tab::Audio), &state, palette),
            Tab::Files => files(&items(&messages, Tab::Files), &state, palette),
            Tab::Links => links(&messages, palette),
        };
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
                        .child(tab_button("audio-tab", "Audio", Tab::Audio))
                        .child(tab_button("documents-tab", "Files", Tab::Files))
                        .child(tab_button("links-tab", "Links", Tab::Links)),
                )
                .child(div().id("all-media-body").debug_selector(|| "all-media-body".into()).h(px(420.)).overflow_y_scroll().child(body)),
        )
    });
}

fn empty(text: &'static str, palette: Palette) -> gpui_kit::AnyElement {
    div().p_6().text_center().text_color(palette.text_secondary).child(text).into_any_element()
}

/// Desktop's `MediaContextMenu`: "View in chat", "Forward", and "Save"
/// once it's downloaded.
fn item_menu(menu: PopupMenu, item: &Item, state: &Entity<AppState>) -> PopupMenu {
    let (show, forward) = (state.clone(), state.clone());
    let (conversation, message_id) = (item.conversation.clone(), item.message_id);
    let menu = menu
        .item(PopupMenuItem::new("View in chat").icon(IconName::MessageSquareShare).on_click(move |_, window, cx| {
            window.close_all_dialogs(cx);
            let conversation = conversation.clone();
            show.update(cx, |s, cx| s.jump_to(conversation, message_id, cx));
        }))
        .item(PopupMenuItem::new("Forward").icon(IconName::Forward).on_click(move |_, window, cx| {
            window.close_all_dialogs(cx);
            crate::views::forward::open(forward.clone(), vec![message_id], window, cx);
        }));
    match item.attachment.local_path.clone() {
        Some(path) => {
            let name = save_name(&item.attachment);
            menu.item(
                PopupMenuItem::new("Save")
                    .icon(IconName::Download)
                    .on_click(move |_, _, cx| crate::views::lightbox::save_as(path.clone().into(), name.clone(), cx)),
            )
        }
        None => menu,
    }
}

/// The sent file name, else the stored file's.
fn save_name(attachment: &AttachmentMeta) -> String {
    attachment.file_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| {
        let path = std::path::PathBuf::from(attachment.local_path.clone().unwrap_or_default());
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into())
    })
}

/// Downloaded photos, opening in the lightbox. (Videos and GIFs need
/// decoding to show, so only photos are drawn here.)
fn media_grid(items: &[Item], state: &Entity<AppState>, palette: Palette) -> gpui_kit::AnyElement {
    let tiles: Vec<(String, Item)> = items
        .iter()
        .filter(|i| i.attachment.content_type.starts_with("image/"))
        .filter_map(|i| Some((i.attachment.local_path.clone()?, i.clone())))
        .collect();
    if tiles.is_empty() {
        return empty("No media", palette);
    }
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(tiles.into_iter().enumerate().map(|(ix, (path, item))| {
            let (open_state, menu_state) = (state.clone(), state.clone());
            let viewed = ViewedImage {
                path: path.clone().into(),
                size: item.attachment.width.zip(item.attachment.height),
                file_name: item.attachment.file_name.clone(),
                view_once: false,
                in_chat: Some(InChat::new(item.conversation.clone(), item.message_id, item.index)),
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
                    open_state.update(cx, |_, cx| cx.emit(AppEvent::ViewImage(viewed)));
                })
                .child(img(std::path::PathBuf::from(path)).size_full().object_fit(ObjectFit::Cover))
                .context_menu(move |menu, _, _| item_menu(menu, &item, &menu_state))
        }))
        .into_any_element()
}

/// Desktop's `AudioListItem`: "file name · sender" (or the sender), then
/// size, "Voice Message" or "Audio", and when. A click plays it in the
/// mini player, on its own (or downloads it first).
fn audio_list(items: &[Item], state: &Entity<AppState>, palette: Palette) -> gpui_kit::AnyElement {
    if items.is_empty() {
        return empty("No audio", palette);
    }
    div()
        .flex()
        .flex_col()
        .children(items.iter().enumerate().map(|(ix, item)| {
            let a = &item.attachment;
            let title = match &a.file_name {
                Some(name) if !name.is_empty() => format!("{name} · {}", item.sender),
                _ => item.sender.clone(),
            };
            let subtitle = format!(
                "{} · {} · {}",
                format_size(a.size),
                if a.voice { "Voice Message" } else { "Audio" },
                short_timestamp(item.sent_at)
            );
            let (play_state, menu_state, menu_item) = (state.clone(), state.clone(), item.clone());
            let (message_id, index, path, sender) = (item.message_id, item.index, a.local_path.clone(), item.sender.clone());
            div()
                .id(("audio-row", ix))
                .debug_selector(move || format!("audio-row-{ix}"))
                .flex()
                .items_center()
                .gap_3()
                .py_1p5()
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(move |_, _, cx| match &path {
                    Some(path) => {
                        let path = std::path::PathBuf::from(path);
                        play_state.update(cx, |s, cx| s.play_audio_alone(message_id, index, path, sender.clone(), cx));
                    }
                    None => play_state.read(cx).send(Command::FetchAttachment { message_id, index }),
                })
                .child(
                    div()
                        .size(px(36.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .bg(palette.input_bg)
                        .child(Icon::new(if a.local_path.is_some() { IconName::Play } else { IconName::Download }).size_4()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(SharedString::from(title)))
                        .child(div().text_sm().truncate().text_color(palette.text_secondary).child(SharedString::from(subtitle))),
                )
                .context_menu(move |menu, _, _| item_menu(menu, &menu_item, &menu_state))
        }))
        .into_any_element()
}

/// Files that aren't photos, videos, audio, GIFs or stickers.
fn files(items: &[Item], state: &Entity<AppState>, palette: Palette) -> gpui_kit::AnyElement {
    if items.is_empty() {
        return empty("No files", palette);
    }
    div()
        .flex()
        .flex_col()
        .children(items.iter().enumerate().map(|(ix, item)| {
            let (menu_state, menu_item) = (state.clone(), item.clone());
            div()
                .id(("file-row", ix))
                .flex()
                .justify_between()
                .py_1p5()
                .child(div().truncate().child(item.attachment.file_name.clone().unwrap_or_else(|| "Attachment".into())))
                .child(div().text_sm().text_color(palette.text_secondary).child(short_timestamp(item.sent_at)))
                .context_menu(move |menu, _, _| item_menu(menu, &menu_item, &menu_state))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(content_type: &str) -> AttachmentMeta {
        AttachmentMeta {
            content_type: content_type.into(),
            file_name: None,
            size: 1,
            width: None,
            height: None,
            local_path: None,
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        }
    }

    #[test]
    fn attachments_go_in_desktops_tabs() {
        assert_eq!(tab_of(&attachment("image/png")), Some(Tab::Media));
        assert_eq!(tab_of(&attachment("video/mp4")), Some(Tab::Media));
        assert_eq!(tab_of(&attachment("audio/mpeg")), Some(Tab::Audio));
        assert_eq!(tab_of(&attachment("application/pdf")), Some(Tab::Files));
        assert_eq!(tab_of(&attachment("text/x-signal-plain")), None);
        let voice = AttachmentMeta { voice: true, ..attachment("audio/aac") };
        assert_eq!(tab_of(&voice), Some(Tab::Audio));
        let sticker = AttachmentMeta { sticker: true, ..attachment("image/webp") };
        assert_eq!(tab_of(&sticker), None);
        let gif = AttachmentMeta { gif: true, ..attachment("video/mp4") };
        assert_eq!(tab_of(&gif), None);
    }

    #[gpui_kit::test]
    fn the_audio_tab_lists_and_plays_voice_messages_on_their_own(cx: &mut gpui_kit::TestAppContext) {
        use crate::views::test_support::{click_at, demo, press, shows, wait};
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        let maya = ConversationId("d:00000000-0000-4000-8000-00000000a11c".into());
        wait(cx, "Maya's chat", &mut |cx| state.read_with(cx, |s, _| s.conversation(&maya).is_some()));
        state.update(cx, |s, _| s.open_audio = crate::audio::silent::open);
        cx.update(|window, cx| open(state.clone(), maya.clone(), window, cx));
        press(cx, "audio-tab");
        wait(cx, "her two voice messages", &mut |cx| shows(cx, "audio-row-1"));
        click_at(cx, "audio-row-1");
        let playing = state.read_with(cx, |s, _| s.audio.as_ref().map(|a| a.message_id));
        assert!(playing.is_some());
        // When it ends, nothing follows it.
        wait(cx, "the end", &mut |cx| {
            cx.executor().advance_clock(std::time::Duration::from_millis(60));
            state.read_with(cx, |s, _| s.audio.is_none())
        });
        for _ in 0..20 {
            cx.executor().advance_clock(std::time::Duration::from_millis(60));
            cx.run_until_parked();
        }
        assert!(state.read_with(cx, |s, _| s.audio.is_none()));
        backend.shutdown();
    }
}
