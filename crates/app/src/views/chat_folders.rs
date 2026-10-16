//! Chat folders in the chat list, as Signal Desktop has them: the folders
//! above the chats with their unread counts and menus
//! (`ts/components/leftPane/LeftPaneChatFolders.dom.tsx`), and a chat's
//! "Add to folder" and "Remove from folder"
//! (`LeftPaneConversationListItemContextMenu.dom.tsx`).

use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Context, Entity, FontWeight, SharedString, TestSupportExt as _, Window, div, px};
use signal_backend::{ChatChange, Command, MUTED_ALWAYS};
use signal_store::{ChatFolder, ChatFolderEdit, ChatFolderType, Conversation};

use crate::state::AppState;
use crate::theme::Palette;

/// Desktop's `UNREAD_BADGE_MAX_COUNT`.
const UNREAD_BADGE_MAX: u32 = 999;

/// The chats in `folder`, archived ones aside (Desktop counts the inbox).
pub fn chats_in<'a>(state: &'a AppState, folder: &ChatFolder) -> Vec<&'a Conversation> {
    state.conversations.iter().filter(|c| AppState::in_folder(folder, c)).collect()
}

/// A folder's unread count for its badge (Desktop's `countUnreadStats`
/// with its default badge setting): unread messages in chats that aren't
/// muted, a chat marked unread counting one.
pub fn unread_count(state: &AppState, folder: &ChatFolder) -> u32 {
    chats_in(state, folder)
        .into_iter()
        .filter(|c| !c.muted)
        .map(|c| if c.unread_count > 0 { c.unread_count } else { u32::from(c.marked_unread) })
        .sum()
}

/// Desktop's labels: "All chats" for the All folder, else its name.
pub fn label(folder: &ChatFolder) -> String {
    match folder.folder_type {
        ChatFolderType::All => "All chats".into(),
        _ => folder.name.clone(),
    }
}

/// The folders above the chat list, once there's a custom one: the chosen
/// one highlighted, each with its unread count and right-click menu.
pub fn tabs(state: &Entity<AppState>, palette: Palette, cx: &gpui_kit::App) -> Option<AnyElement> {
    let s = state.read(cx);
    if !s.has_custom_folders() {
        return None;
    }
    let selected = s.selected_folder().map(|f| f.id.clone());
    let folders = s.current_folders();
    let items = folders.into_iter().enumerate().map(|(ix, folder)| {
        let chosen = match &selected {
            Some(id) => *id == folder.id,
            None => folder.folder_type == ChatFolderType::All,
        };
        let unread = unread_count(s, &folder);
        let (pick_state, pick) = (state.clone(), (folder.folder_type != ChatFolderType::All).then(|| folder.id.clone()));
        let (menu_state, menu_folder) = (state.clone(), folder.clone());
        div()
            .id(SharedString::from(format!("chat-folder-tab-{ix}")))
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2p5()
            .py_1()
            .rounded_full()
            .text_sm()
            .cursor_pointer()
            .when(chosen, |d| d.bg(palette.row_selected).font_weight(FontWeight::SEMIBOLD))
            .when(!chosen, |d| d.hover(|d| d.bg(palette.row_hover)))
            .child(div().max_w(px(120.)).truncate().child(label(&folder)))
            .when(unread > 0, |d| {
                let text = if unread > UNREAD_BADGE_MAX { format!("{UNREAD_BADGE_MAX}+") } else { unread.to_string() };
                d.child(div().px_1p5().rounded_full().bg(palette.badge).text_color(palette.on_accent).text_xs().child(text))
            })
            .on_click(move |_, _, cx| pick_state.update(cx, |s, cx| s.select_folder(pick.clone(), cx)))
            .test_support()
            .context_menu(move |menu, window, cx| tab_menu(menu, &menu_folder, &menu_state, window, cx))
    });
    Some(
        div()
            .id("chat-folders")
            .debug_selector(|| "chat-folders".into())
            .flex()
            .gap_1()
            .px_3()
            .pb_2()
            .overflow_x_scroll()
            .children(items)
            .into_any_element(),
    )
}

/// Desktop's folder menu: "Mark all read" when anything's unread, "Mute
/// notifications" (or "Unmute all" when all are muted), and "Edit folder"
/// for a custom one.
fn tab_menu(menu: PopupMenu, folder: &ChatFolder, state: &Entity<AppState>, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let s = state.read(cx);
    let chats: Vec<Conversation> = chats_in(s, folder).into_iter().cloned().collect();
    let unread = chats.iter().any(|c| c.unread_count > 0 || c.marked_unread);
    let muted = chats.iter().any(|c| c.muted);
    let unmuted = chats.iter().any(|c| !c.muted);
    let mut menu = menu;
    if unread {
        let (state, chats) = (state.clone(), chats.clone());
        menu = menu.item(PopupMenuItem::new("Mark all read").icon(IconName::CheckCheck).on_click(move |_, _, cx| {
            for chat in chats.iter().filter(|c| c.unread_count > 0 || c.marked_unread) {
                let s = state.read(cx);
                if chat.marked_unread {
                    s.set_chat(chat.id.clone(), ChatChange::MarkedUnread(false));
                }
                s.send(Command::MarkRead { conversation: chat.id.clone() });
            }
        }));
    }
    let mute_all = |until: i64, state: Entity<AppState>, chats: Vec<Conversation>| {
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
            for chat in &chats {
                if (until == 0) == chat.muted {
                    state.read(cx).set_chat(chat.id.clone(), ChatChange::MutedUntil(until));
                }
            }
        }
    };
    if muted && !unmuted {
        menu = menu.item(PopupMenuItem::new("Unmute all").icon(IconName::Bell).on_click(mute_all(0, state.clone(), chats.clone())));
    } else {
        let (state, chats) = (state.clone(), chats.clone());
        menu = menu.submenu_with_icon(Some(IconName::BellOff.into()), "Mute notifications", window, cx, move |menu, _, _| {
            let mut menu = menu;
            for (label, ms) in crate::views::chat_menu::MUTE_FOR {
                let (state, chats) = (state.clone(), chats.clone());
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    let until = chrono::Utc::now().timestamp_millis() + ms;
                    for chat in chats.iter().filter(|c| !c.muted) {
                        state.read(cx).set_chat(chat.id.clone(), ChatChange::MutedUntil(until));
                    }
                }));
            }
            let (always_state, always_chats) = (state.clone(), chats.clone());
            menu = menu.item(PopupMenuItem::new("Always").on_click(move |_, _, cx| {
                for chat in always_chats.iter().filter(|c| !c.muted) {
                    always_state.read(cx).set_chat(chat.id.clone(), ChatChange::MutedUntil(MUTED_ALWAYS));
                }
            }));
            if muted {
                let (state, chats) = (state.clone(), chats.clone());
                menu = menu.separator().item(PopupMenuItem::new("Unmute all").on_click(move |_, _, cx| {
                    for chat in chats.iter().filter(|c| c.muted) {
                        state.read(cx).set_chat(chat.id.clone(), ChatChange::MutedUntil(0));
                    }
                }));
            }
            menu
        });
    }
    if folder.folder_type == ChatFolderType::Custom {
        let (state, id) = (state.clone(), folder.id.clone());
        menu = menu.item(PopupMenuItem::new("Edit folder").icon(IconName::Pencil).on_click(move |_, window, cx| {
            crate::views::settings::edit_folder(state.clone(), Some(id.clone()), None, window, cx)
        }));
    }
    menu
}

/// A chat's folder items in the chat list's menu: with "All chats" shown,
/// "Add to folder" (each custom folder, checked when it's in, and "Create
/// folder"); with a custom folder shown, "Remove from folder".
pub fn chat_menu_items(
    menu: PopupMenu,
    conversation: &Conversation,
    state: &Entity<AppState>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let s = state.read(cx);
    if !s.has_custom_folders() {
        return menu;
    }
    match s.selected_folder().filter(|f| f.folder_type == ChatFolderType::Custom).cloned() {
        Some(folder) => {
            let (state, id, chat) = (state.clone(), folder.id.clone(), conversation.id.clone());
            menu.item(PopupMenuItem::new("Remove from folder").icon(IconName::FolderDown).on_click(move |_, _, cx| {
                let edit = ChatFolderEdit::ToggleChat { id: id.clone(), chat: chat.clone(), add: false };
                state.update(cx, |s, cx| s.edit_chat_folders(edit, cx));
            }))
        }
        None => {
            let folders: Vec<ChatFolder> =
                s.current_folders().into_iter().filter(|f| f.folder_type == ChatFolderType::Custom).collect();
            let (state, conversation) = (state.clone(), conversation.clone());
            menu.submenu_with_icon(Some(IconName::Folder.into()), "Add to folder", window, cx, move |mut menu, _, _| {
                let chat = AppState::folder_chat(&conversation);
                for folder in &folders {
                    let checked = folder.contains_with(&chat, true);
                    // Desktop: a muted chat can't join a folder that hides muted chats.
                    let muted_out = !folder.show_muted_chats && conversation.muted;
                    let (state, id, chat_id) = (state.clone(), folder.id.clone(), conversation.id.clone());
                    menu = menu.item(
                        PopupMenuItem::new(folder.name.clone()).checked(checked).disabled(checked || muted_out).on_click(move |_, _, cx| {
                            let edit = ChatFolderEdit::ToggleChat { id: id.clone(), chat: chat_id.clone(), add: true };
                            state.update(cx, |s, cx| s.edit_chat_folders(edit, cx));
                        }),
                    );
                }
                let (state, chat_id) = (state.clone(), conversation.id.clone());
                menu.separator().item(PopupMenuItem::new("Create folder").on_click(move |_, window, cx| {
                    let params = signal_store::ChatFolderParams { included: vec![chat_id.clone()], ..signal_store::ChatFolderParams::defaults() };
                    crate::views::settings::edit_folder(state.clone(), None, Some(params), window, cx)
                }))
            })
        }
    }
}
