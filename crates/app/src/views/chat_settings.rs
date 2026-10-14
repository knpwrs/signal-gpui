//! Desktop's "Group settings" (name, description, members, leave) and
//! "Chat settings" (who the chat is with).

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, FontWeight, SharedString, Window, div, px};
use signal_backend::Command;
use signal_store::{Conversation, ConversationKind};

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::{conversation_avatar, person_avatar};

pub fn open(state: Entity<AppState>, conversation: Conversation, window: &mut Window, cx: &mut App) {
    let group = conversation.kind == ConversationKind::Group;
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
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let editable = group && conversation.can_edit_info && !conversation.left;
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
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(conversation.title.clone()))
                    .when_some(conversation.phone.clone().filter(|_| !group), |d, phone| {
                        d.child(div().text_sm().text_color(palette.text_secondary).child(phone))
                    }),
            );
        let body = div().flex().flex_col().gap_4().child(header);
        if !group {
            let direct = conversation.kind == ConversationKind::Direct;
            let (state, chat) = (state.clone(), conversation.clone());
            let safety = Button::new("view-safety-number").label("View safety number").outline().on_click(move |_, window, cx| {
                window.close_all_dialogs(cx);
                crate::views::safety_number::open(state.clone(), chat.id.clone(), chat.title.clone(), window, cx);
            });
            let body = body.when(direct, |d| d.child(div().child(safety)));
            return dialog.title("Chat settings").w(px(420.)).child(body);
        }
        let members = names.read(cx).clone();
        let member_rows = members.into_iter().map(|(aci, name)| {
            let admin = conversation.admins.contains(&aci);
            let you = aci == me;
            div()
                .flex()
                .items_center()
                .gap_3()
                .py_1()
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
        dialog.title("Group settings").w(px(460.)).child(
            body.when(editable, |d| {
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
            .when(conversation.can_label && !conversation.left, |d| {
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
            .child(field("Members"))
            .child(div().id("group-members").max_h(px(260.)).overflow_y_scroll().children(member_rows))
            .when(!conversation.left, |d| {
                d.child(Button::new("group-leave").label("Leave group").danger().on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    confirm_leave(leave_state.clone(), leave_id.clone(), window, cx);
                }))
            }),
        )
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
