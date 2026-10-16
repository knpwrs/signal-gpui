//! Desktop's "Group settings" (photo, name, description, members, the
//! group link, requests and invites, permissions, leave) and "Chat
//! settings" (who the chat is with, and groups in common).

use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, FontWeight, SharedString, Window, div, px};
use signal_backend::Command;
use gpui_kit::component::tooltip::Tooltip;
use signal_store::{ContactNickname, Conversation, ConversationKind};

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::{conversation_avatar, person_avatar};

pub fn open(state: Entity<AppState>, conversation: Conversation, window: &mut Window, cx: &mut App) {
    let group = conversation.kind == ConversationKind::Group;
    // Desktop fetches the group as its details open.
    if group {
        state.read(cx).send(Command::RefreshGroup { conversation: conversation.id.clone() });
    }
    let title = cx.new(|cx| InputState::new(window, cx).default_value(conversation.title.clone()));
    let description =
        cx.new(|cx| InputState::new(window, cx).placeholder("Group description").default_value(conversation.description.clone().unwrap_or_default()));
    let me_now = state.read(cx).account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
    let mine = conversation.member_labels.get(&me_now).cloned();
    let label_emoji = cx.new(|cx| {
        InputState::new(window, cx).placeholder("Emoji").default_value(mine.as_ref().and_then(|l| l.emoji.clone()).unwrap_or_default())
    });
    let label_text = cx.new(|cx| {
        InputState::new(window, cx).placeholder("Your label").default_value(mine.as_ref().map(|l| l.text.clone()).unwrap_or_default())
    });
    // Member names load from the database.
    let names: Entity<Vec<(String, String)>> = cx.new(|_| Vec::new());
    if group {
        let backend = state.read(cx).backend.clone();
        let db = backend.db().clone();
        let members = conversation.members.clone();
        let query = backend.spawn(async move { db.display_names(&members).await });
        let names = names.clone();
        let all_members = conversation.members.clone();
        cx.spawn(async move |cx| {
            // Everyone, named or not yet.
            let known: std::collections::HashMap<String, String> = query.await.unwrap_or_default().into_iter().collect();
            let mut found: Vec<(String, String)> = all_members
                .into_iter()
                .map(|aci| {
                    let name = known.get(&aci).cloned().unwrap_or_else(|| "Unknown".to_owned());
                    (aci, name)
                })
                .collect();
            found.sort_by_key(|(_, name)| name.to_lowercase());
            names.update(cx, |names, cx| {
                *names = found;
                cx.notify();
            });
        })
        .detach();
    }
    // The nickname and note we gave them, from the database.
    let nickname: Entity<Option<ContactNickname>> = cx.new(|_| None);
    if crate::views::nickname::can_have_nickname(&conversation)
        && let Some(sid) = conversation.id.as_direct().map(str::to_owned)
    {
        let backend = state.read(cx).backend.clone();
        let db = backend.db().clone();
        let query = backend.spawn(async move { db.contact_nickname(&sid).await });
        let nickname = nickname.clone();
        cx.spawn(async move |cx| {
            let found = query.await.ok().flatten().unwrap_or_default();
            nickname.update(cx, |nickname, cx| {
                *nickname = Some(found);
                cx.notify();
            });
        })
        .detach();
    }
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let editable = group && conversation.can_edit_info && !conversation.left;
        let nickname = nickname.read(cx).clone();
        // Desktop's "Nickname (profile name)", when a nickname is what shows.
        let plain_name = nickname
            .as_ref()
            .filter(|n| n.nickname().is_some())
            .and_then(|n| n.plain_name.clone())
            .filter(|plain| *plain != conversation.title);
        let me = state.read(cx).account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
        let field = |label: &'static str| div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child(label);
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .child(conversation_avatar(&conversation, px(56.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .text_lg()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(conversation.title.clone()))
                            .when_some(plain_name, |d, plain| {
                                let tip = format!("“{plain}” is the profile name this person set for themselves in Signal.");
                                d.child(
                                    div()
                                        .id("nickname-plain-name")
                                        .debug_selector(|| "nickname-plain-name".into())
                                        .text_color(palette.text_secondary)
                                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                                        .child(format!("({plain})")),
                                )
                            }),
                    )
                    .when_some(conversation.phone.clone().filter(|_| !group), |d, phone| {
                        d.child(div().text_sm().text_color(palette.text_secondary).child(phone))
                    }),
            );
        let body = div().flex().flex_col().gap_4().child(header);
        if !group {
            let direct = conversation.kind == ConversationKind::Direct;
            let (safety_state, chat) = (state.clone(), conversation.clone());
            let safety = Button::new("view-safety-number").label("View safety number").outline().on_click(move |_, window, cx| {
                window.close_all_dialogs(cx);
                crate::views::safety_number::open(safety_state.clone(), chat.id.clone(), chat.title.clone(), window, cx);
            });
            let body = body.when(direct, |d| d.child(div().child(safety)));
            let body = body.child(div().child(chat_color_button(&state, &conversation.id, cx)));
            let body = match nickname {
                Some(nickname) if crate::views::nickname::can_have_nickname(&conversation) => {
                    body.child(nickname_rows(state.clone(), conversation.clone(), nickname, palette))
                }
                _ => body,
            };
            // Desktop's `ConversationDetailsGroups`.
            let body = match conversation.id.as_direct().filter(|_| direct && !conversation.id.is_signal()) {
                Some(aci) => body.child(crate::views::group_admin::groups_in_common_section(&state, aci, palette, cx)),
                None => body,
            };
            return dialog.title("Chat settings").w(px(420.)).child(body);
        }
        // The group as it is now: these dialogs stay open while it changes.
        let conversation = state.read(cx).conversation(&conversation.id).cloned().unwrap_or_else(|| conversation.clone());
        let members = names.read(cx).clone();
        let member_rows = members.into_iter().filter(|(aci, _)| conversation.members.contains(aci)).map(|(aci, name)| {
            let admin = conversation.admins.contains(&aci);
            let you = aci == me;
            let (member_state, member_group, member) = (state.clone(), conversation.id.clone(), aci.clone());
            let selector = format!("member-{aci}");
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector)
                .flex()
                .items_center()
                .gap_3()
                .py_1()
                .px_1()
                .rounded_md()
                // Desktop's `ContactModal`, for anyone but us.
                .when(!you, |d| {
                    d.cursor_pointer().hover(|s| s.bg(palette.row_hover)).on_click(move |_, window, cx| {
                        crate::views::group_admin::open_member(member_state.clone(), member_group.clone(), member.clone(), window, cx)
                    })
                })
                .child(person_avatar(state.read(cx).photo_of(&aci), &name, color_for(&aci), px(28.)))
                .child(div().flex_1().truncate().child(if you { "You".to_owned() } else { name }))
                .when_some(conversation.member_labels.get(&aci).cloned(), |d, label| {
                    d.child(div().text_xs().text_color(palette.text_secondary).child(match label.emoji {
                        Some(emoji) => format!("{emoji} {}", label.text),
                        None => label.text,
                    }))
                })
                .when(admin, |d| d.child(div().text_xs().text_color(palette.text_secondary).child("Admin")))
        });
        let (save_state, save_id, title_input, description_input) =
            (state.clone(), conversation.id.clone(), title.clone(), description.clone());
        let (original_title, original_description) = (conversation.title.clone(), conversation.description.clone().unwrap_or_default());
        let (leave_state, leave_id) = (state.clone(), conversation.id.clone());
        // Desktop's `canTerminateGroup`: admins, with `groupTerminate.send`.
        let can_end = !conversation.left
            && !conversation.ended
            && conversation.admins.contains(&me)
            && signal_backend::remote_config().group_terminate_send();
        let (end_state, end_chat) = (state.clone(), conversation.clone());
        let admin = crate::views::group_admin::we_are_admin(&conversation, &me);
        let in_group = !conversation.left && !conversation.ended;
        let link_on = conversation.group_state.link.is_some();
        let pending = crate::views::group_admin::pending_count(&conversation);
        let panel = |id: &'static str, label: String| Button::new(id).label(label).outline().small();
        let (link_state, panels_id) = (state.clone(), conversation.id.clone());
        let (requests_state, requests_id) = (state.clone(), conversation.id.clone());
        let (permissions_state, permissions_id) = (state.clone(), conversation.id.clone());
        // Desktop's `ConversationDetails` rows: the group link (admins, or
        // anyone while it's on), requests and invites, and (admins)
        // permissions.
        let panels = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .when(in_group && (admin || link_on), |d| {
                d.child(panel("group-link", format!("Group link: {}", if link_on { "On" } else { "Off" })).on_click(move |_, window, cx| {
                    crate::views::group_admin::open_link(link_state.clone(), panels_id.clone(), window, cx)
                }))
            })
            .when(in_group, |d| {
                d.child(panel("group-requests-invites", format!("Requests & invites ({pending})")).on_click(move |_, window, cx| {
                    crate::views::group_admin::open_requests(requests_state.clone(), requests_id.clone(), window, cx)
                }))
            })
            .when(in_group && admin, |d| {
                d.child(panel("group-permissions", "Permissions".into()).on_click(move |_, window, cx| {
                    crate::views::group_admin::open_permissions(permissions_state.clone(), permissions_id.clone(), window, cx)
                }))
            });
        // Desktop's avatar picker in `EditConversationAttributesModal`.
        let photo = editable.then(|| {
            let (pick_state, pick_id) = (state.clone(), conversation.id.clone());
            let (remove_state, remove_id) = (state.clone(), conversation.id.clone());
            div()
                .flex()
                .gap_2()
                .child(Button::new("group-photo-change").label("Change photo").outline().small().on_click(move |_, window, cx| {
                    crate::views::group_admin::pick_avatar(pick_state.clone(), pick_id.clone(), window, cx)
                }))
                .when(conversation.avatar_path.is_some(), |d| {
                    d.child(Button::new("group-photo-remove").label("Remove photo").ghost().small().on_click(move |_, _, cx| {
                        remove_state.read(cx).send(Command::SetGroupAvatar { conversation: remove_id.clone(), image: None });
                    }))
                })
        });
        dialog.title("Group settings").w(px(460.)).child(
            // Desktop's `ConversationDetails__GroupTerminatedBanner`.
            body.when(conversation.ended, |d| {
                d.child(
                    div()
                        .id("group-ended-banner")
                        .debug_selector(|| "group-ended-banner".into())
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(palette.row_hover)
                        .text_sm()
                        .text_center()
                        .child("This group has ended"),
                )
            })
            .when_some(photo, |d, photo| d.child(photo))
            .when(editable, |d| {
                d.child(div().flex().flex_col().gap_1().child(field("Group name")).child(Input::new(&title)))
                    .child(div().flex().flex_col().gap_1().child(field("Description")).child(Input::new(&description)))
                    .child(div().flex().justify_end().child(Button::new("group-save").label("Save").primary().on_click(
                        move |_, window, cx| {
                            let title = title_input.read(cx).value().trim().to_string();
                            let description = description_input.read(cx).value().trim().to_string();
                            let title = (title != original_title && !title.is_empty()).then_some(title);
                            let description = (description != original_description).then_some(description);
                            if title.is_some() || description.is_some() {
                                save_state.read(cx).send(Command::UpdateGroup { conversation: save_id.clone(), title, description });
                            }
                            window.close_all_dialogs(cx);
                        },
                    )))
            })
            .when(!editable, |d| {
                d.when_some(conversation.description.clone(), |d, text| {
                    d.child(div().text_sm().text_color(palette.text_secondary).child(SharedString::from(text)))
                })
            })
            // Desktop's `isEditMemberLabelEnabled` (`groupMemberLabels.edit`).
            .when(conversation.can_label && !conversation.left && signal_backend::remote_config().group_member_labels_edit(), |d| {
                let (state, id, emoji, text) = (state.clone(), conversation.id.clone(), label_emoji.clone(), label_text.clone());
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(field("Your label"))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(div().w(px(72.)).child(Input::new(&label_emoji)))
                                .child(div().flex_1().child(Input::new(&label_text)))
                                .child(Button::new("label-save").label("Set").outline().on_click(move |_, window, cx| {
                                    let emoji = Some(emoji.read(cx).value().trim().to_string()).filter(|e| !e.is_empty());
                                    let text = text.read(cx).value().trim().to_string();
                                    state.read(cx).send(Command::SetMemberLabel { conversation: id.clone(), emoji, text });
                                    window.close_all_dialogs(cx);
                                })),
                        )
                        .child(div().text_xs().text_color(palette.text_secondary).child("Shown next to your name in this group. Up to 24 characters; leave it empty to remove it.")),
                )
            })
            .child(panels)
            .child(div().child(chat_color_button(&state, &conversation.id, cx)))
            .child(field("Members"))
            // Desktop's `canAddNewMembers`.
            .when(conversation.can_add_members && !conversation.left && !conversation.ended, |d| {
                let (state, chat) = (state.clone(), conversation.clone());
                d.child(div().child(Button::new("group-add-members").label("Add members").outline().on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    crate::views::new_group::open_add_members(state.clone(), chat.clone(), window, cx);
                })))
            })
            .child(div().id("group-members").max_h(px(260.)).overflow_y_scroll().children(member_rows))
            // Desktop doesn't offer leaving an ended group.
            .when(!conversation.left && !conversation.ended, |d| {
                d.child(Button::new("group-leave").label("Leave group").danger().on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    confirm_leave(leave_state.clone(), leave_id.clone(), window, cx);
                }))
            })
            .when(can_end, |d| {
                d.child(div().debug_selector(|| "group-end".into()).child(Button::new("group-end").label("End group").danger().on_click(
                    move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        confirm_end(end_state.clone(), end_chat.clone(), window, cx);
                    },
                )))
            }),
        )
    });
}

/// Desktop's "Chat color" row in a chat's details, with its swatch.
fn chat_color_button(state: &Entity<AppState>, chat: &signal_store::ConversationId, cx: &App) -> Button {
    let color = state.read(cx).chat_colors.of(chat).clone();
    let (state, chat) = (state.clone(), chat.clone());
    Button::new("chat-color")
        .outline()
        .small()
        .child(div().flex().items_center().gap_2().child(crate::views::chat_colors::swatch_for(&color)).child("Chat color"))
        .on_click(move |_, window, cx| crate::views::chat_colors::open_for_chat(state.clone(), chat.clone(), window, cx))
}

/// Desktop's "Nickname" row (with "Delete" once there's one) and the note,
/// which opens in full.
fn nickname_rows(state: Entity<AppState>, conversation: Conversation, nickname: ContactNickname, palette: Palette) -> impl IntoElement {
    let (edit_state, edit_chat, edit_nickname) = (state.clone(), conversation.clone(), nickname.clone());
    let (delete_state, delete_id) = (state.clone(), conversation.id.clone());
    let has_any = !nickname.is_empty();
    let note = Some(nickname.note.clone()).filter(|n| !n.is_empty());
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .gap_2()
                .child(Button::new("nickname-edit").label("Nickname").outline().on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    crate::views::nickname::open_edit(edit_state.clone(), edit_chat.clone(), edit_nickname.clone(), window, cx);
                }))
                .when(has_any, |d| {
                    d.child(Button::new("nickname-delete").label("Delete").ghost().on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        crate::views::nickname::confirm_delete(delete_state.clone(), delete_id.clone(), window, cx);
                    }))
                }),
        )
        .when_some(note, |d, note| {
            d.child(
                div()
                    .id("nickname-note")
                    .debug_selector(|| "nickname-note".into())
                    .text_sm()
                    .p_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(palette.row_hover))
                    .child(div().text_xs().text_color(palette.text_secondary).child("Note"))
                    .child(div().line_clamp(3).child(SharedString::from(note)))
                    .on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        crate::views::nickname::open_note(state.clone(), conversation.clone(), nickname.clone(), window, cx);
                    }),
            )
        })
}

/// Desktop's two confirmations before ending a group
/// (`ConversationDetailsActions`): what it means, then whether you're sure.
pub fn confirm_end(state: Entity<AppState>, conversation: Conversation, window: &mut Window, cx: &mut App) {
    let title = format!("End \"{}\"?", conversation.title);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, id) = (state.clone(), conversation.id.clone());
        alert
            .title(SharedString::from(title.clone()))
            .description(
                "Members will no longer be able to send messages or start calls in the group. They will be notified that you ended the group, and will still have access to message history.",
            )
            .confirm()
            .ok_text("End group")
            .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                window.close_all_dialogs(cx);
                let (state, id) = (state.clone(), id.clone());
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (state, id) = (state.clone(), id.clone());
                    alert
                        .description("This will end the group permanently. Are you sure you want to proceed?")
                        .confirm()
                        .ok_text("End group")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .on_ok(move |_, window, cx| {
                            state.read(cx).send(Command::EndGroup { conversation: id.clone() });
                            window.close_all_dialogs(cx);
                            true
                        })
                });
                false
            })
    });
}

/// Desktop's "Do you really want to leave?".
pub fn confirm_leave(state: Entity<AppState>, conversation: signal_store::ConversationId, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, conversation) = (state.clone(), conversation.clone());
        alert
            .title("Do you really want to leave?")
            .description("You will no longer be able to send or receive messages in this group.")
            .confirm()
            .ok_text("Leave")
            .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.read(cx).send(Command::LeaveGroup { conversation: conversation.clone() });
                window.close_all_dialogs(cx);
                true
            })
    });
}
