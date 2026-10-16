//! Desktop's chat folder pages (`ts/components/preferences/chatFolders/`):
//! the list (`PreferencesChatFoldersPage`: the folders, "Create a folder",
//! the suggested ones) and a folder's editor
//! (`PreferencesEditChatFoldersPage`, with `PreferencesSelectChatsDialog`).
//! Desktop reorders folders by dragging them; here a folder's menu moves it
//! up or down.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window, div, px};
use signal_store::{ChatFolderEdit, ChatFolderParams, ChatFolderType, ConversationId};

use super::{Page, Settings, go, list, switch};
use crate::state::AppState;
use crate::theme::Palette;

/// A new folder's id: a random UUID, as Desktop's `generateUuid`.
pub(crate) fn new_folder_id() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// The Chats page's "Chat folders" row: "Add a chat folder", or "Add or
/// edit folders" with how many there are.
pub(super) fn row(settings: &Entity<Settings>, palette: Palette, cx: &App) -> AnyElement {
    let state = settings.read(cx).state.read(cx);
    let custom = state.current_folders().into_iter().filter(|f| f.folder_type == ChatFolderType::Custom).count();
    let (label, value) = if custom > 0 {
        ("Add or edit folders", Some(SharedString::from(if custom == 1 { "1 folder".to_owned() } else { format!("{custom} folders") })))
    } else {
        ("Add a chat folder", None)
    };
    let settings = settings.clone();
    super::link(
        "chat-folders-settings",
        label,
        Some("Organize your chats into folders and quickly switch between them on your chat list.".into()),
        value,
        false,
        palette,
        move |window, cx| go(&settings, Page::ChatFolders, window, cx),
    )
}

/// Desktop's suggested folders, by preset.
fn suggestions() -> [(&'static str, &'static str, &'static str, ChatFolderParams); 3] {
    [
        ("chat-folder-suggest-unread", "Unread", "Unread messages from all chats", ChatFolderParams::unread()),
        ("chat-folder-suggest-direct", "1:1 chats", "Only messages from direct chats", ChatFolderParams::individual()),
        ("chat-folder-suggest-groups", "Groups", "Only messages from group chats", ChatFolderParams::groups()),
    ]
}

/// Desktop's chat folders page.
pub(super) fn render_list(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let folders = state.read(cx).current_folders();
    let ids: Vec<String> = folders.iter().map(|f| f.id.clone()).collect();
    let create_settings = settings.clone();
    let create = div()
        .id("chat-folder-create")
        .flex()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .child(Icon::new(IconName::Plus).size_4())
        .child(div().text_sm().child("Create a folder"))
        .on_click(move |_, window, cx| open_editor(&create_settings, None, None, window, cx))
        .test_support()
        .into_any_element();
    let rows = folders.iter().enumerate().map(|(ix, folder)| {
        let custom = folder.folder_type == ChatFolderType::Custom;
        let (edit_settings, id) = (settings.clone(), folder.id.clone());
        let (menu_settings, menu_state, menu_id, menu_name, menu_ids) =
            (settings.clone(), state.clone(), folder.id.clone(), folder.name.clone(), ids.clone());
        let label = crate::views::chat_folders::label(folder);
        div()
            .id(SharedString::from(format!("chat-folder-{ix}")))
            .flex()
            .items_center()
            .gap_3()
            .when(custom, |d| {
                d.cursor_pointer().on_click(move |_, window, cx| open_editor(&edit_settings, Some(id.clone()), None, window, cx))
            })
            .child(Icon::new(IconName::Folder).size_4().text_color(palette.text_secondary))
            .child(div().flex_1().min_w_0().truncate().text_sm().child(label))
            .test_support()
            .context_menu(move |menu, _, _| {
                let mut menu = menu;
                let position = menu_ids.iter().position(|i| *i == menu_id).unwrap_or(0);
                let moved = |to: usize| {
                    let mut order = menu_ids.clone();
                    let id = order.remove(position);
                    order.insert(to, id);
                    order
                };
                if position > 0 {
                    let (state, order) = (menu_state.clone(), moved(position - 1));
                    menu = menu.item(PopupMenuItem::new("Move up").on_click(move |_, _, cx| {
                        state.update(cx, |s, cx| s.edit_chat_folders(ChatFolderEdit::Reorder(order.clone()), cx))
                    }));
                }
                if position + 1 < menu_ids.len() {
                    let (state, order) = (menu_state.clone(), moved(position + 1));
                    menu = menu.item(PopupMenuItem::new("Move down").on_click(move |_, _, cx| {
                        state.update(cx, |s, cx| s.edit_chat_folders(ChatFolderEdit::Reorder(order.clone()), cx))
                    }));
                }
                if custom {
                    let (settings, id) = (menu_settings.clone(), menu_id.clone());
                    menu = menu.item(PopupMenuItem::new("Edit folder").icon(IconName::Pencil).on_click(move |_, window, cx| {
                        open_editor(&settings, Some(id.clone()), None, window, cx)
                    }));
                    let (state, id, name) = (menu_state.clone(), menu_id.clone(), menu_name.clone());
                    menu = menu.item(PopupMenuItem::new("Delete").icon(IconName::Trash).on_click(move |_, window, cx| {
                        confirm_delete(state.clone(), id.clone(), &name, None, window, cx)
                    }));
                }
                menu
            })
            .into_any_element()
    });
    let suggested: Vec<AnyElement> = suggestions()
        .into_iter()
        .filter(|(_, _, _, preset)| !folders.iter().any(|f| preset.matches(f)))
        .map(|(id, title, description, preset)| {
            let state = state.clone();
            super::action(
                title,
                Some(description.into()),
                Button::new(id).label("Add").outline().on_click(move |_, window, cx| {
                    let params = ChatFolderParams { name: title.into(), ..preset.clone() };
                    let edit = ChatFolderEdit::Create { id: new_folder_id(), params };
                    state.update(cx, |s, cx| s.edit_chat_folders(edit, cx));
                    // Desktop's "{name} folder added" toast.
                    window.push_notification(gpui_kit::component::notification::Notification::new().message(format!("{title} folder added")), cx);
                }),
                palette,
            )
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(div().text_sm().text_color(palette.text_secondary).child(
            "Organize your chats into folders and quickly switch between them on your chat list",
        ))
        .child(list(Some("Folders"), None, std::iter::once(create).chain(rows), palette))
        .when(!suggested.is_empty(), |d| d.child(list(Some("Suggested folders"), None, suggested, palette)))
        .into_any_element()
}

/// Desktop's delete confirmations: from the list ("Delete folder?") or the
/// editor ("Delete this chat folder?", which then goes back).
fn confirm_delete(state: Entity<AppState>, id: String, name: &str, editor: Option<Entity<Settings>>, window: &mut Window, cx: &mut App) {
    let (title, description) = if editor.is_some() {
        ("Delete this chat folder?".to_owned(), "This folder will be removed from your chat list.".to_owned())
    } else {
        ("Delete folder?".to_owned(), format!("Do you want to delete the folder “{name}”?"))
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, id, editor) = (state.clone(), id.clone(), editor.clone());
        alert
            .title(title.clone())
            .description(description.clone())
            .confirm()
            .ok_text("Delete")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                let at = chrono::Utc::now().timestamp_millis().max(1) as u64;
                state.update(cx, |s, cx| s.edit_chat_folders(ChatFolderEdit::Delete { id: id.clone(), at }, cx));
                if let Some(settings) = &editor {
                    settings.update(cx, |s, _| s.folder_editor = None);
                    go(settings, Page::ChatFolders, window, cx);
                }
                true
            })
    });
}

/// A folder being made or edited (Desktop's `PreferencesEditChatFoldersPage`
/// state): what Save would save, against what it started as.
pub struct FolderEditor {
    pub existing: Option<String>,
    pub params: ChatFolderParams,
    initial: ChatFolderParams,
    pub name: Entity<InputState>,
    _subscription: Subscription,
}

impl FolderEditor {
    fn new(existing: Option<String>, initial: ChatFolderParams, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name (required)").default_value(initial.name.clone()));
        let subscription = cx.subscribe(&name, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.params.name = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        Self { existing, params: initial.clone(), initial, name, _subscription: subscription }
    }

    fn normalized(params: &ChatFolderParams) -> ChatFolderParams {
        ChatFolderParams { name: params.name.trim().to_owned(), ..params.clone() }
    }

    /// Desktop's `isChanged`.
    pub fn changed(&self) -> bool {
        Self::normalized(&self.params) != Self::normalized(&self.initial)
    }

    /// Saves (`createChatFolder` or `updateChatFolder`); `false` when it
    /// can't be saved (Desktop's `isValid`) or nothing changed.
    pub fn save(editor: &Entity<Self>, state: &Entity<AppState>, window: &mut Window, cx: &mut App) -> bool {
        let this = editor.read(cx);
        if !this.params.valid() || !this.changed() {
            return false;
        }
        let params = Self::normalized(&this.params);
        let existing = this.existing.clone();
        let edit = match &existing {
            Some(id) => ChatFolderEdit::Update { id: id.clone(), params },
            None => {
                window.push_notification(
                    gpui_kit::component::notification::Notification::new().message(format!("{} folder added", params.name)),
                    cx,
                );
                ChatFolderEdit::Create { id: new_folder_id(), params }
            }
        };
        state.update(cx, |s, cx| s.edit_chat_folders(edit, cx));
        true
    }
}

/// Opens the editor in Settings: an existing folder, or a new one (from
/// `init`, Desktop's `initChatFolderParams`).
fn open_editor(settings: &Entity<Settings>, existing: Option<String>, init: Option<ChatFolderParams>, window: &mut Window, cx: &mut App) {
    let state = settings.read(cx).state.clone();
    let initial = match &existing {
        Some(id) => match state.read(cx).chat_folders.iter().find(|f| &f.id == id) {
            Some(folder) => folder.params(),
            None => return,
        },
        None => init.unwrap_or_else(ChatFolderParams::defaults),
    };
    let editor = cx.new(|cx| FolderEditor::new(existing, initial, window, cx));
    settings.update(cx, |s, _| s.folder_editor = Some(editor));
    go(settings, Page::EditChatFolder, window, cx);
}

/// Opens Settings at a folder's editor (from the chat list).
pub fn edit_folder(state: Entity<AppState>, existing: Option<String>, init: Option<ChatFolderParams>, window: &mut Window, cx: &mut App) {
    if let Some(settings) = super::show(state, Page::ChatFolders, window, cx) {
        open_editor(&settings, existing, init, window, cx);
    }
}

/// Leaving the editor: Desktop asks "Save changes?" when there are some.
pub(super) fn leave_editor(settings: &Entity<Settings>, window: &mut Window, cx: &mut App) {
    let editor = settings.read(cx).folder_editor.clone();
    let Some(editor) = editor.filter(|e| e.read(cx).changed()) else {
        go(settings, Page::ChatFolders, window, cx);
        return;
    };
    let settings = settings.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let valid = editor.read(cx).params.valid();
        let (discard_settings, save_settings, save_editor) = (settings.clone(), settings.clone(), editor.clone());
        dialog
            .title("Save changes?")
            .w(px(400.))
            .child(div().text_sm().child("Do you want to save the changes you’ve made to this chat folder?"))
            .footer(
                DialogFooter::new()
                    .child(Button::new("chat-folder-discard").label("Discard").outline().on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        go(&discard_settings, Page::ChatFolders, window, cx);
                    }))
                    .child(Button::new("chat-folder-save-changes").label("Save").primary().disabled(!valid).on_click(move |_, window, cx| {
                        let state = save_settings.read(cx).state.clone();
                        window.close_dialog(cx);
                        if FolderEditor::save(&save_editor, &state, window, cx) {
                            go(&save_settings, Page::ChatFolders, window, cx);
                        }
                    })),
            )
    });
}

/// A chat's row in the editor's lists, with a button to take it out.
fn chat_row(
    id: SharedString,
    chat: &ConversationId,
    state: &AppState,
    on_remove: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let title = state.conversation(chat).map_or_else(|| "Unknown".to_owned(), |c| c.title.clone());
    div()
        .flex()
        .items_center()
        .gap_3()
        .children(state.conversation(chat).map(|c| crate::views::common::conversation_avatar(c, px(28.))))
        .child(div().flex_1().min_w_0().truncate().text_sm().child(title))
        .child(
            Button::new(id)
                .ghost()
                .small()
                .icon(IconName::X)
                .tooltip("Remove")
                .on_click(move |_, window, cx| on_remove(window, cx)),
        )
        .into_any_element()
}

/// Desktop's folder editor page.
pub(super) fn render_edit(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let Some(editor) = settings.read(cx).folder_editor.clone() else {
        return div().into_any_element();
    };
    let state = settings.read(cx).state.clone();
    let (params, existing) = {
        let e = editor.read(cx);
        (e.params.clone(), e.existing.clone())
    };
    let s = state.read(cx);
    let edit = |change: Box<dyn Fn(&mut ChatFolderParams)>| {
        let editor = editor.clone();
        move |window: &mut Window, cx: &mut App| {
            editor.update(cx, |e, cx| {
                change(&mut e.params);
                cx.notify();
            });
            window.refresh();
        }
    };
    let mut included: Vec<AnyElement> = Vec::new();
    if params.include_all_individual_chats {
        let remove = edit(Box::new(|p| p.include_all_individual_chats = false));
        included.push(type_row("chat-folder-remove-direct", "1:1 chats", IconName::User, palette, remove));
    }
    if params.include_all_group_chats {
        let remove = edit(Box::new(|p| p.include_all_group_chats = false));
        included.push(type_row("chat-folder-remove-groups", "Group chats", IconName::Users, palette, remove));
    }
    for (ix, chat) in params.included.iter().enumerate() {
        let chat_id = chat.clone();
        let remove = edit(Box::new(move |p| p.included.retain(|c| *c != chat_id)));
        included.push(chat_row(SharedString::from(format!("chat-folder-remove-included-{ix}")), chat, s, remove));
    }
    let mut excluded: Vec<AnyElement> = Vec::new();
    for (ix, chat) in params.excluded.iter().enumerate() {
        let chat_id = chat.clone();
        let remove = edit(Box::new(move |p| p.excluded.retain(|c| *c != chat_id)));
        excluded.push(chat_row(SharedString::from(format!("chat-folder-remove-excluded-{ix}")), chat, s, remove));
    }
    let (include_state, include_editor) = (state.clone(), editor.clone());
    let add_chats = Button::new("chat-folder-add-chats").label("Add chats").outline().small().on_click(move |_, window, cx| {
        select_chats(include_state.clone(), include_editor.clone(), true, window, cx)
    });
    let (exclude_state, exclude_editor) = (state.clone(), editor.clone());
    let exclude_chats = Button::new("chat-folder-exclude-chats").label("Exclude chats").outline().small().on_click(move |_, window, cx| {
        select_chats(exclude_state.clone(), exclude_editor.clone(), false, window, cx)
    });
    let (unread_editor, muted_editor) = (editor.clone(), editor.clone());
    let filters = [
        switch(
            "chat-folder-only-unread",
            "Only show unread chats",
            Some("Only chats with unread messages will be shown in this folder.".into()),
            params.show_only_unread,
            false,
            palette,
            move |on, _, cx| unread_editor.update(cx, |e, cx| {
                e.params.show_only_unread = on;
                cx.notify();
            }),
        ),
        switch("chat-folder-include-muted", "Include muted chats", None, params.show_muted_chats, false, palette, move |on, _, cx| {
            muted_editor.update(cx, |e, cx| {
                e.params.show_muted_chats = on;
                cx.notify();
            })
        }),
    ];
    let can_save = params.valid() && editor.read(cx).changed();
    let (cancel_settings, save_settings, save_editor, save_state) = (settings.clone(), settings.clone(), editor.clone(), state.clone());
    let delete = existing.clone().map(|id| {
        let (state, name) = (state.clone(), params.name.clone());
        let settings = settings.clone();
        Button::new("chat-folder-delete").label("Delete folder").danger().small().on_click(move |_, window, cx| {
            confirm_delete(state.clone(), id.clone(), &name, Some(settings.clone()), window, cx)
        })
    });
    div()
        .debug_selector(|| "chat-folder-editor".into())
        .flex()
        .flex_col()
        .gap_5()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child("Folder name"))
                .child(Input::new(&editor.read(cx).name)),
        )
        .child(list(
            Some("Included chats"),
            Some("Choose chats that you want to appear in this folder".into()),
            std::iter::once(add_chats.into_any_element()).chain(included),
            palette,
        ))
        .child(list(
            Some("Exceptions"),
            Some("Choose chats that you do not want to appear in this folder".into()),
            std::iter::once(exclude_chats.into_any_element()).chain(excluded),
            palette,
        ))
        .child(list(None, None, filters, palette))
        .children(delete.map(|d| div().child(d)))
        .child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(Button::new("chat-folder-cancel").label("Cancel").outline().on_click(move |_, window, cx| {
                    cancel_settings.update(cx, |s, _| s.folder_editor = None);
                    go(&cancel_settings, Page::ChatFolders, window, cx);
                }))
                .child(Button::new("chat-folder-save").label("Save").primary().disabled(!can_save).on_click(move |_, window, cx| {
                    if FolderEditor::save(&save_editor, &save_state, window, cx) {
                        save_settings.update(cx, |s, _| s.folder_editor = None);
                        go(&save_settings, Page::ChatFolders, window, cx);
                    }
                })),
        )
        .into_any_element()
}

fn type_row(id: &'static str, label: &'static str, icon: IconName, palette: Palette, on_remove: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(div().size(px(28.)).flex().items_center().justify_center().rounded_full().bg(palette.row_hover).child(Icon::new(icon).size_4()))
        .child(div().flex_1().text_sm().child(label))
        .child(Button::new(id).ghost().small().icon(IconName::X).tooltip("Remove").on_click(move |_, window, cx| on_remove(window, cx)))
        .into_any_element()
}

/// Desktop's `PreferencesSelectChatsDialog`: chat types (for included
/// chats), then every chat, checked when chosen; Done keeps them.
struct ChatPicker {
    search: Entity<InputState>,
    direct: bool,
    groups: bool,
    chosen: Vec<ConversationId>,
    _subscription: Subscription,
}

fn select_chats(state: Entity<AppState>, editor: Entity<FolderEditor>, including: bool, window: &mut Window, cx: &mut App) {
    let params = editor.read(cx).params.clone();
    let picker = cx.new(|cx| {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify());
        ChatPicker {
            search,
            direct: params.include_all_individual_chats,
            groups: params.include_all_group_chats,
            chosen: if including { params.included.clone() } else { params.excluded.clone() },
            _subscription: subscription,
        }
    });
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let p = picker.read(cx);
        let query = p.search.read(cx).value().to_lowercase();
        let s = state.read(cx);
        let chats: Vec<signal_store::Conversation> = s
            .conversations
            .iter()
            .chain(&s.archived)
            .filter(|c| !c.id.is_signal() && (query.is_empty() || c.title.to_lowercase().contains(&query)))
            .cloned()
            .collect();
        let toggle_type = |id: &'static str, label: &'static str, on: bool, group: bool| {
            let picker = picker.clone();
            Checkbox::new(id).label(label).checked(on).on_click(move |on, _, cx| {
                picker.update(cx, |p, cx| {
                    if group {
                        p.groups = *on;
                    } else {
                        p.direct = *on;
                    }
                    cx.notify();
                })
            })
        };
        let rows = chats.iter().enumerate().map(|(ix, chat)| {
            let (picker, id) = (picker.clone(), chat.id.clone());
            let checked = p.chosen.contains(&chat.id);
            div()
                .flex()
                .items_center()
                .gap_3()
                .py_1()
                .child(crate::views::common::conversation_avatar(chat, px(28.)))
                .child(div().flex_1().min_w_0().truncate().text_sm().child(chat.title.clone()))
                .child(Checkbox::new(SharedString::from(format!("chat-folder-pick-{ix}"))).checked(checked).on_click(move |on, _, cx| {
                    picker.update(cx, |p, cx| {
                        p.chosen.retain(|c| *c != id);
                        if *on {
                            p.chosen.push(id.clone());
                        }
                        cx.notify();
                    })
                }))
        });
        let (done_picker, done_editor) = (picker.clone(), editor.clone());
        dialog
            .title(if including { "Included chats" } else { "Excluded chats" })
            .w(px(420.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(Input::new(&p.search))
                    .when(including && query.is_empty(), |d| {
                        d.child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child("Chat types"))
                            .child(toggle_type("chat-folder-pick-direct", "1:1 chats", p.direct, false))
                            .child(toggle_type("chat-folder-pick-groups", "Groups", p.groups, true))
                    })
                    .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child("Recent chats"))
                    .child(div().id("chat-folder-picks").max_h(px(320.)).overflow_y_scroll().children(rows)),
            )
            .footer(DialogFooter::new().child(Button::new("chat-folder-pick-done").label("Done").primary().on_click(move |_, window, cx| {
                let p = done_picker.read(cx);
                let (direct, groups, chosen) = (p.direct, p.groups, p.chosen.clone());
                done_editor.update(cx, |e, cx| {
                    if including {
                        e.params.include_all_individual_chats = direct;
                        e.params.include_all_group_chats = groups;
                        e.params.included = chosen.clone();
                        e.params.excluded.retain(|c| !chosen.contains(c));
                    } else {
                        e.params.excluded = chosen.clone();
                        e.params.included.retain(|c| !chosen.contains(c));
                    }
                    cx.notify();
                });
                window.close_dialog(cx);
                window.refresh();
            })))
    });
}
