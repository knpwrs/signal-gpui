//! Group administration, as Signal Desktop shows it: a member's details
//! and admin actions (`ContactModal`, `RemoveGroupMemberConfirmationDialog`),
//! permissions (`GroupV2Permissions`), the group link
//! (`GroupLinkManagement`), requests and invitations (`PendingInvites`),
//! what an invited or waiting member sees instead of the composer
//! (`MessageRequestActions`, `GroupV2PendingApprovalActions`), the
//! announcement-only banner (`AnnouncementsOnlyGroupBanner`), joining by a
//! link (`GroupV2JoinDialog`), the group photo, and groups in common
//! (`ConversationDetailsGroups`).

use std::collections::HashMap;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, SharedString, Window, div, px};
use signal_backend::Command;
use signal_backend::group_admin::{LinkChange, Permission};
use signal_store::{Conversation, ConversationId, ConversationKind, Membership};

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::{conversation_avatar, person_avatar};

/// Names for service ids, loaded from the database (who isn't known by
/// name shows as "Unknown", as in Desktop).
pub(crate) fn load_names(state: &Entity<AppState>, ids: Vec<String>, cx: &mut App) -> Entity<HashMap<String, String>> {
    let names: Entity<HashMap<String, String>> = cx.new(|_| HashMap::new());
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let query = backend.spawn(async move { db.display_names(&ids).await });
    let loaded = names.clone();
    cx.spawn(async move |cx| {
        let found: HashMap<String, String> = query.await.unwrap_or_default().into_iter().collect();
        loaded.update(cx, |names, cx| {
            *names = found;
            cx.notify();
        });
    })
    .detach();
    names
}

fn name_in(names: &HashMap<String, String>, sid: &str) -> String {
    names.get(sid).cloned().unwrap_or_else(|| "Unknown".to_owned())
}

fn our_aci(state: &Entity<AppState>, cx: &App) -> String {
    state.read(cx).account.as_ref().map(|a| a.aci.clone()).unwrap_or_default()
}

/// The group as it is now (dialogs stay open while it changes).
fn current(state: &Entity<AppState>, id: &ConversationId, cx: &App) -> Option<Conversation> {
    state.read(cx).conversation(id).cloned()
}

/// Whether we're an admin of a group we're in.
pub(crate) fn we_are_admin(conversation: &Conversation, me: &str) -> bool {
    !conversation.left && !conversation.ended && conversation.admins.iter().any(|a| a == me)
}

fn confirm(
    title: Option<String>,
    description: String,
    ok: &'static str,
    danger: bool,
    window: &mut Window,
    cx: &mut App,
    then: impl Fn(&mut Window, &mut App) + 'static,
) {
    let then = std::rc::Rc::new(then);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let then = then.clone();
        let alert = match &title {
            Some(title) => alert.title(SharedString::from(title.clone())),
            None => alert,
        };
        alert
            .description(SharedString::from(description.clone()))
            .confirm()
            .ok_text(ok)
            .ok_variant(if danger { ButtonVariant::Danger } else { ButtonVariant::Primary })
            .on_ok(move |_, window, cx| {
                then(window, cx);
                true
            })
    });
}

/// Desktop's `ContactModal` for someone in a group: who they are, ways to
/// reach them, and (for admins) making them an admin or removing them.
pub fn open_member(state: Entity<AppState>, group: ConversationId, member: String, window: &mut Window, cx: &mut App) {
    let names = load_names(&state, vec![member.clone()], cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let me = our_aci(&state, cx);
        let Some(chat) = current(&state, &group, cx) else { return dialog };
        let name = name_in(names.read(cx), &member);
        let direct = ConversationId::direct(&member);
        let is_admin = chat.admins.contains(&member);
        let in_group = chat.members.contains(&member);
        let admin_actions = we_are_admin(&chat, &me) && in_group && member != me;
        let label = chat.member_labels.get(&member).cloned();
        let row_button = |id: &'static str, label: &'static str| Button::new(id).label(label).outline().small();
        let (message_state, message_to) = (state.clone(), direct.clone());
        let message = row_button("member-message", "Message").on_click(move |_, window, cx| {
            window.close_all_dialogs(cx);
            let entry = signal_store::DirectoryEntry {
                id: message_to.clone(),
                kind: ConversationKind::Direct,
                title: String::new(),
                phone: None,
            };
            message_state.update(cx, |s, cx| s.start_conversation(entry, cx));
        });
        let call = |id: &'static str, label: &'static str, video: bool| {
            let (state, to) = (state.clone(), direct.clone());
            row_button(id, label).on_click(move |_, window, cx| {
                window.close_all_dialogs(cx);
                state.update(cx, |s, cx| s.open_call_lobby(to.clone(), video, cx));
            })
        };
        let (safety_state, safety_to, safety_name) = (state.clone(), direct.clone(), name.clone());
        let safety = row_button("member-safety-number", "View safety number").on_click(move |_, window, cx| {
            window.close_all_dialogs(cx);
            crate::views::safety_number::open(safety_state.clone(), safety_to.clone(), safety_name.clone(), window, cx);
        });
        // Desktop's "Nickname": for someone we have a chat with.
        let nickname_chat = current(&state, &direct, cx).filter(crate::views::nickname::can_have_nickname);
        let mut body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(person_avatar(state.read(cx).photo_of(&member), &name, color_for(&member), px(80.)))
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(name.clone()))
                    .when_some(label, |d, label| {
                        d.child(div().text_sm().text_color(palette.text_secondary).child(match label.emoji {
                            Some(emoji) => format!("{emoji} {}", label.text),
                            None => label.text,
                        }))
                    })
                    .when(is_admin, |d| d.child(div().text_xs().text_color(palette.text_secondary).child("Admin"))),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .gap_2()
                    .child(message)
                    .child(call("member-voice", "Voice", false))
                    .child(call("member-video", "Video", true)),
            )
            .child(div().flex().flex_col().gap_2().child(safety));
        if let Some(chat_with) = nickname_chat {
            let (nick_state, nick_chat) = (state.clone(), chat_with.clone());
            body = body.child(div().child(row_button("member-nickname", "Nickname").on_click(move |_, window, cx| {
                window.close_all_dialogs(cx);
                let (state, chat) = (nick_state.clone(), nick_chat.clone());
                let backend = state.read(cx).backend.clone();
                let db = backend.db().clone();
                let sid = chat.id.as_direct().unwrap_or_default().to_owned();
                let query = backend.spawn(async move { db.contact_nickname(&sid).await });
                window
                    .spawn(cx, async move |cx| {
                        let found = query.await.ok().flatten().unwrap_or_default();
                        let _ = cx.update(|window, cx| crate::views::nickname::open_edit(state, chat, found, window, cx));
                    })
                    .detach();
            })));
        }
        if admin_actions {
            let (admin_state, admin_group, admin_member, admin_name) = (state.clone(), group.clone(), member.clone(), name.clone());
            let labels_go = label_goes_with_admin(&chat, &member);
            let toggle = Button::new("member-toggle-admin")
                .label(if is_admin { "Remove as admin" } else { "Make admin" })
                .outline()
                .small()
                .on_click(move |_, window, cx| {
                    let description = if is_admin {
                        let mut text = format!("Remove {admin_name} as group admin?");
                        if labels_go {
                            text.push_str(" This will also clear their member label.");
                        }
                        text
                    } else {
                        format!("{admin_name} will be able to edit this group and its members.")
                    };
                    let (state, group, member) = (admin_state.clone(), admin_group.clone(), admin_member.clone());
                    confirm(None, description, if is_admin { "Remove as admin" } else { "Make admin" }, false, window, cx, move |window, cx| {
                        state.read(cx).send(Command::SetGroupAdmin { conversation: group.clone(), member: member.clone(), admin: !is_admin });
                        window.close_all_dialogs(cx);
                    });
                });
            let (remove_state, remove_group, remove_member, remove_name) = (state.clone(), group.clone(), member.clone(), name.clone());
            let with_link = chat.group_state.link.is_some();
            let remove = Button::new("member-remove").label("Remove from group").danger().small().on_click(move |_, window, cx| {
                confirm_remove(remove_state.clone(), remove_group.clone(), remove_member.clone(), &remove_name, with_link, window, cx);
            });
            body = body.child(div().flex().flex_col().gap_2().child(toggle).child(remove));
        }
        dialog.w(px(380.)).child(body)
    });
}

/// Desktop's `buildModifyMemberRoleChange`: an admin's label goes with the
/// role when only admins may have labels.
fn label_goes_with_admin(chat: &Conversation, member: &str) -> bool {
    chat.admins.iter().any(|a| a == member) && chat.member_labels.contains_key(member) && chat.group_state.labels_admins_only
}

/// Desktop's `RemoveGroupMemberConfirmationDialog`.
pub fn confirm_remove(
    state: Entity<AppState>,
    group: ConversationId,
    member: String,
    name: &str,
    with_link: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let description = if with_link {
        format!("Remove \"{name}\" from the group? They will not be able to rejoin via the group link.")
    } else {
        format!("Remove \"{name}\" from the group?")
    };
    confirm(None, description, "Remove from group", true, window, cx, move |window, cx| {
        state.read(cx).send(Command::RemoveFromGroup { conversation: group.clone(), service_id: member.clone() });
        window.close_all_dialogs(cx);
    });
}

/// "All members" or "Only Admins", as Desktop's permission selects offer.
fn access_choice(
    id: &'static str,
    admins_only: bool,
    state: &Entity<AppState>,
    group: &ConversationId,
    permission: impl Fn(bool) -> Permission + 'static,
) -> impl IntoElement {
    let permission = std::rc::Rc::new(permission);
    let option = |label: &'static str, value: bool| {
        let (state, group, permission) = (state.clone(), group.clone(), permission.clone());
        let selector = format!("{id}-{}", if value { "admins" } else { "all" });
        let button = Button::new(SharedString::from(selector.clone())).label(label).small();
        let button = if admins_only == value { button.primary() } else { button.outline() };
        div().debug_selector(move || selector).child(button.on_click(move |_, _, cx| {
            if admins_only != value {
                state.read(cx).send(Command::SetGroupPermission { conversation: group.clone(), permission: permission(value) });
            }
        }))
    };
    div().flex().gap_1().child(option("All members", false)).child(option("Only Admins", true))
}

/// Desktop's `GroupV2Permissions`: who may edit details, add people, and
/// send messages. Admins only.
pub fn open_permissions(state: Entity<AppState>, group: ConversationId, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let Some(chat) = current(&state, &group, cx) else { return dialog };
        let g = &chat.group_state;
        let row = |label: &'static str, info: &'static str, choice: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                .child(div().text_xs().text_color(palette.text_secondary).child(info))
                .child(choice)
        };
        dialog.title("Permissions").w(px(440.)).child(
            div()
                .flex()
                .flex_col()
                .gap_4()
                .child(row(
                    "Who can add members",
                    "Choose who can add members to this group.",
                    access_choice("permission-add-members", g.members_admins_only, &state, &group, |admins_only| Permission::AddMembers {
                        admins_only,
                    })
                    .into_any_element(),
                ))
                .child(row(
                    "Who can edit group info",
                    "Choose who can edit group name, photo, description, disappearing messages timer, and pinned messages.",
                    access_choice("permission-edit-info", g.info_admins_only, &state, &group, |admins_only| Permission::EditInfo {
                        admins_only,
                    })
                    .into_any_element(),
                ))
                .child(row(
                    "Who can send messages",
                    "Choose who can send messages to the group.",
                    access_choice("permission-send", g.announcements_only, &state, &group, |admins_only| Permission::SendMessages {
                        admins_only,
                    })
                    .into_any_element(),
                )),
        )
    });
}

/// Desktop's `GroupLinkManagement`: admins turn the link on or off, reset
/// it and choose approval; anyone may copy it while it's on.
pub fn open_link(state: Entity<AppState>, group: ConversationId, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let me = our_aci(&state, cx);
        let Some(chat) = current(&state, &group, cx) else { return dialog };
        let admin = we_are_admin(&chat, &me);
        let link = chat.group_state.link.clone();
        let change = |change: LinkChange| {
            let (state, group) = (state.clone(), group.clone());
            move |cx: &mut App| state.read(cx).send(Command::ChangeGroupLink { conversation: group.clone(), change })
        };
        let toggle_on = change(LinkChange::On);
        let toggle_off = change(LinkChange::Off);
        let mut body = div().flex().flex_col().gap_4().child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Group link"))
                .when(admin, |d| {
                    d.child(div().debug_selector(|| "group-link-toggle".into()).child(
                        Switch::new("group-link-toggle").checked(link.is_some()).on_click(move |on: &bool, _, cx| {
                            if *on { toggle_on(cx) } else { toggle_off(cx) }
                        }),
                    ))
                }),
        );
        if let Some(link) = link {
            let url = link.url.clone();
            let copy = Button::new("group-link-copy").label("Copy link").outline().small().on_click(move |_, window, cx| {
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(url.clone()));
                window.push_notification(gpui_kit::component::notification::Notification::new().message("Copied group link to clipboard"), cx);
            });
            body = body
                .child(
                    div()
                        .id("group-link-url")
                        .debug_selector(|| "group-link-url".into())
                        .w_full()
                        .truncate()
                        .text_sm()
                        .text_color(palette.text_secondary)
                        .child(SharedString::from(link.url.clone())),
                )
                .child(div().flex().gap_2().child(copy).when(admin, |d| {
                    let reset = change(LinkChange::Reset);
                    d.child(Button::new("group-link-reset").label("Reset link").outline().small().on_click(move |_, window, cx| {
                        let reset = reset.clone();
                        confirm(
                            None,
                            "Are you sure you want to reset the group link? People will no longer be able to join the group using the current link."
                                .into(),
                            "Reset link",
                            false,
                            window,
                            cx,
                            move |_, cx| reset(cx),
                        );
                    }))
                }))
                .when(admin, |d| {
                    let approval = change(LinkChange::Approval(!link.approval));
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .child(div().child("Require admin approval"))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(palette.text_secondary)
                                            .child("Require an admin to approve new members joining via the group link"),
                                    ),
                            )
                            .child(div().debug_selector(|| "group-link-approval".into()).child(
                                Switch::new("group-link-approval").checked(link.approval).on_click(move |_: &bool, _, cx| approval(cx)),
                            )),
                    )
                })
                // The link as a QR code, for a phone to scan.
                .child(
                    div()
                        .flex()
                        .justify_center()
                        .child(div().bg(gpui_kit::white()).p_2().child(crate::views::link::qr_code(link.url.as_bytes(), 160.))),
                );
        }
        dialog.title("Group link").w(px(440.)).child(body)
    });
}

/// Desktop's `PendingInvites`: requests to join (admins approve or turn
/// them down) and invitations (admins revoke them).
pub fn open_requests(state: Entity<AppState>, group: ConversationId, window: &mut Window, cx: &mut App) {
    let ids = state
        .read(cx)
        .conversation(&group)
        .map(|c| {
            c.group_state
                .requests
                .iter()
                .map(|r| r.aci.clone())
                .chain(c.group_state.invites.iter().flat_map(|i| [i.service_id.clone(), i.by.clone()]))
                .collect()
        })
        .unwrap_or_default();
    let names = load_names(&state, ids, cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let me = our_aci(&state, cx);
        let Some(chat) = current(&state, &group, cx) else { return dialog };
        let admin = we_are_admin(&chat, &me);
        let names = names.read(cx).clone();
        let g = chat.group_state.clone();
        let heading = |text: String| div().text_sm().font_weight(FontWeight::SEMIBOLD).child(text);
        let info = |text: String| div().text_xs().text_color(palette.text_secondary).child(text);
        let person_row = |sid: &str, name: String| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_1()
                .child(person_avatar(state.read(cx).photo_of(sid), &name, color_for(sid), px(28.)))
                .child(div().flex_1().truncate().child(name))
        };
        let with_link = g.link.is_some();
        let requests = g.requests.iter().map(|request| {
            let name = name_in(&names, &request.aci);
            let answer = |approve: bool| {
                let (state, group, member, name) = (state.clone(), group.clone(), request.aci.clone(), name.clone());
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                    let (description, ok) = if approve {
                        (format!("Approve request from \"{name}\"?"), "Approve Request")
                    } else if with_link {
                        (format!("Deny request from \"{name}\"? They will not be able to request to join via the group link again."), "Deny Request")
                    } else {
                        (format!("Deny request from \"{name}\"?"), "Deny Request")
                    };
                    let (state, group, member) = (state.clone(), group.clone(), member.clone());
                    confirm(None, description, ok, !approve, window, cx, move |_, cx| {
                        state.read(cx).send(Command::AnswerJoinRequest { conversation: group.clone(), member: member.clone(), approve });
                    });
                }
            };
            let selector = format!("request-{}", request.aci);
            person_row(&request.aci, name.clone()).debug_selector(move || selector).when(admin, |d| {
                d.child(Button::new(SharedString::from(format!("deny-{}", request.aci))).label("Delete").ghost().small().on_click(answer(false)))
                    .child(Button::new(SharedString::from(format!("approve-{}", request.aci))).label("Accept").primary().small().on_click(answer(true)))
            })
        });
        let revoke = |invitees: Vec<String>, description: String| {
            let (state, group) = (state.clone(), group.clone());
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                let (state, group, invitees) = (state.clone(), group.clone(), invitees.clone());
                confirm(None, description.clone(), "Revoke", true, window, cx, move |_, cx| {
                    state.read(cx).send(Command::RevokeGroupInvites { conversation: group.clone(), invitees: invitees.clone() });
                });
            }
        };
        // Desktop: ours one by one; others' grouped by who sent them.
        let ours = g.invites.iter().filter(|i| i.by == me).map(|invite| {
            let name = name_in(&names, &invite.service_id);
            let selector = format!("invite-{}", invite.service_id);
            person_row(&invite.service_id, name.clone()).debug_selector(move || selector).when(admin, |d| {
                d.child(
                    Button::new(SharedString::from(format!("revoke-{}", invite.service_id)))
                        .label("Revoke")
                        .ghost()
                        .small()
                        .on_click(revoke(vec![invite.service_id.clone()], format!("Revoke group invite for \"{name}\"?"))),
                )
            })
        });
        let mut by_inviter: Vec<(String, Vec<String>)> = Vec::new();
        for invite in g.invites.iter().filter(|i| i.by != me) {
            match by_inviter.iter_mut().find(|(by, _)| *by == invite.by) {
                Some((_, list)) => list.push(invite.service_id.clone()),
                None => by_inviter.push((invite.by.clone(), vec![invite.service_id.clone()])),
            }
        }
        let others = by_inviter.into_iter().map(|(by, invitees)| {
            let name = name_in(&names, &by);
            let count = invitees.len();
            let description = if count == 1 {
                format!("Revoke group invite for \"{}\"?", name_in(&names, &invitees[0]))
            } else {
                format!("Revoke {count} invites sent by \"{name}\"?")
            };
            person_row(&by, name.clone())
                .child(div().text_xs().text_color(palette.text_secondary).child(format!("Invited {count}")))
                .when(admin, |d| {
                    d.child(
                        Button::new(SharedString::from(format!("revoke-by-{by}")))
                            .label("Revoke")
                            .ghost()
                            .small()
                            .on_click(revoke(invitees.clone(), description.clone())),
                    )
                })
        });
        let has_ours = g.invites.iter().any(|i| i.by == me);
        let has_others = g.invites.iter().any(|i| i.by != me);
        dialog.title("Requests & invites").w(px(460.)).child(
            div()
                .id("group-requests")
                .max_h(px(520.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_3()
                .child(heading(format!("Requests ({})", g.requests.len())))
                .children(requests)
                .when(admin || !g.requests.is_empty(), |d| {
                    d.child(info(format!("People on this list are attempting to join \"{}\" via the group link.", chat.title)))
                })
                .child(heading(format!("Invites ({})", g.invites.len())))
                .when(has_ours, |d| d.child(info("Invited by you".into())).children(ours))
                .when(has_others, |d| d.child(info("Invited by others".into())).children(others))
                .child(info(
                    "Details about people invited to this group aren’t shown until they join. Invitees will only see messages after they join the group."
                        .into(),
                )),
        )
    });
}

/// Desktop's admin view of "Requests & invites": how many are waiting.
pub fn pending_count(chat: &Conversation) -> usize {
    chat.group_state.requests.len() + chat.group_state.invites.len()
}

/// Desktop's avatar picker, at its most basic: pick an image file for the
/// group's photo.
pub fn pick_avatar(state: Entity<AppState>, group: ConversationId, window: &mut Window, cx: &mut App) {
    let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Choose a group photo".into()),
    });
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            match std::fs::read(&path) {
                Ok(image) => {
                    let _ = cx.update(|_, cx| state.read(cx).send(Command::SetGroupAvatar { conversation: group, image: Some(image) }));
                }
                Err(error) => {
                    let _ = cx.update(|_, cx| state.update(cx, |s, cx| s.report_error(format!("Couldn't read that image: {error}"), cx)));
                }
            }
        })
        .detach();
}

/// What a group shows instead of the composer, as Desktop does: an
/// invitation to accept, a request to join waiting on an admin, or (for
/// members who aren't admins) the announcement-only banner. `None`: the
/// composer.
pub fn composer_replacement(state: &Entity<AppState>, chat: &Conversation, palette: Palette, cx: &App) -> Option<AnyElement> {
    if chat.kind != ConversationKind::Group || chat.blocked || chat.ended {
        return None;
    }
    let me = our_aci(state, cx);
    let bar = |selector: &'static str| {
        div()
            .id(selector)
            .debug_selector(move || selector.into())
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .px_6()
            .py_4()
            .border_t_1()
            .border_color(palette.panel_border)
            .text_sm()
            .text_center()
    };
    match &chat.group_state.membership {
        // Desktop's `MessageRequestActions` for a group we're invited to.
        Membership::Invited { .. } if !chat.left => {
            let (block_state, block_chat) = (state.clone(), chat.clone());
            let (delete_state, delete_chat) = (state.clone(), chat.clone());
            let (accept_state, accept_id) = (state.clone(), chat.id.clone());
            Some(
                bar("group-invite-actions")
                    .child(div().text_color(palette.text_secondary).child(
                        "Join this group and share your name and photo with its members? They won’t know you’ve seen their messages until you accept.",
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("invite-block").label("Block").outline().on_click(move |_, window, cx| {
                                let (state, id) = (block_state.clone(), block_chat.id.clone());
                                confirm(
                                    Some(format!("Block and Leave {}?", block_chat.title)),
                                    "You will no longer receive messages or updates from this group and members won't be able to add you to this group again."
                                        .into(),
                                    "Block",
                                    true,
                                    window,
                                    cx,
                                    move |_, cx| state.read(cx).send(Command::DeclineGroupInvite { conversation: id.clone(), block: true }),
                                );
                            }))
                            .child(Button::new("invite-delete").label("Delete").outline().on_click(move |_, window, cx| {
                                let (state, id) = (delete_state.clone(), delete_chat.id.clone());
                                confirm(
                                    Some(format!("Delete and Leave {}?", delete_chat.title)),
                                    "You will leave this group, and it will be deleted from all your devices.".into(),
                                    "Delete and Leave",
                                    true,
                                    window,
                                    cx,
                                    move |_, cx| state.read(cx).send(Command::DeclineGroupInvite { conversation: id.clone(), block: false }),
                                );
                            }))
                            .child(Button::new("invite-accept").label("Accept").primary().on_click(move |_, _, cx| {
                                accept_state.read(cx).send(Command::AcceptGroupInvite { conversation: accept_id.clone() });
                            })),
                    )
                    .into_any_element(),
            )
        }
        // Desktop's `GroupV2PendingApprovalActions`.
        Membership::Requesting { .. } => {
            let (state, id) = (state.clone(), chat.id.clone());
            Some(
                bar("group-request-pending")
                    .child(div().text_color(palette.text_secondary).child(
                        "Your request to join has been sent to the group admin. You’ll be notified when they take action.",
                    ))
                    .child(Button::new("cancel-join-request").label("Cancel Request").outline().on_click(move |_, window, cx| {
                        let (state, id) = (state.clone(), id.clone());
                        window.open_alert_dialog(cx, move |alert, _, _| {
                            let (state, id) = (state.clone(), id.clone());
                            alert
                                .description("Cancel your request to join this group?")
                                .confirm()
                                .ok_text("Yes")
                                .cancel_text("No")
                                .on_ok(move |_, _, cx| {
                                    state.read(cx).send(Command::CancelJoinRequest { conversation: id.clone() });
                                    true
                                })
                        });
                    }))
                    .into_any_element(),
            )
        }
        // Desktop's `AnnouncementsOnlyGroupBanner`, with "admins" opening
        // the list of admins to message.
        Membership::Member if chat.group_state.announcements_only && !chat.left && !chat.admins.contains(&me) => {
            let (state, admins) = (state.clone(), chat.admins.clone());
            Some(
                bar("announcements-only-banner")
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .text_color(palette.text_secondary)
                            .child("Only")
                            .child(
                                div()
                                    .id("announcements-admins")
                                    .debug_selector(|| "announcements-admins".into())
                                    .text_color(palette.accent)
                                    .cursor_pointer()
                                    .child("admins")
                                    .on_click(move |_, window, cx| open_admins(state.clone(), admins.clone(), window, cx)),
                            )
                            .child("can send messages"),
                    )
                    .into_any_element(),
            )
        }
        _ => None,
    }
}

/// Desktop's `AnnouncementsOnlyGroupBannerAdminsDialog`: "Message an admin".
fn open_admins(state: Entity<AppState>, admins: Vec<String>, window: &mut Window, cx: &mut App) {
    let names = load_names(&state, admins.clone(), cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let names = names.read(cx).clone();
        let rows = admins.iter().map(|aci| {
            let name = name_in(&names, aci);
            // Before the names load, no title rather than "Unknown": a profile
            // fetch names a new chat.
            let title = names.get(aci).cloned().unwrap_or_default();
            let (state, id) = (state.clone(), ConversationId::direct(aci));
            div()
                .id(SharedString::from(format!("admin-{aci}")))
                .flex()
                .items_center()
                .gap_2()
                .p_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(palette.row_hover))
                .child(person_avatar(state.read(cx).photo_of(aci), &name, color_for(aci), px(28.)))
                .child(name)
                .on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    let entry = signal_store::DirectoryEntry { id: id.clone(), kind: ConversationKind::Direct, title: title.clone(), phone: None };
                    state.update(cx, |s, cx| s.start_conversation(entry, cx));
                })
        });
        dialog.title("Message an admin").w(px(360.)).child(div().flex().flex_col().children(rows))
    });
}

/// Desktop's `GroupV2JoinDialog`: the group a link leads to, and joining
/// (or asking to).
pub fn open_join(state: Entity<AppState>, preview: std::rc::Rc<signal_backend::GroupLinkPreview>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let members = if preview.member_count == 1 { "1 member".to_owned() } else { format!("{} members", preview.member_count) };
        let color = (preview.title.len() % 12) as u8;
        let (join_state, url) = (state.clone(), preview.url.clone());
        let footer = DialogFooter::new()
            .child(Button::new("group-join-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
            .child(
                Button::new("group-join")
                    .label(if preview.approval { "Request to Join" } else { "Join" })
                    .primary()
                    .on_click(move |_, window, cx| {
                        join_state.read(cx).send(Command::JoinGroupLink { url: url.clone() });
                        window.close_all_dialogs(cx);
                    }),
            );
        dialog
            .w(px(380.))
            .child(
                div()
                    .id("group-join-dialog")
                    .debug_selector(|| "group-join-dialog".into())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .text_center()
                    .child(person_avatar(preview.avatar.as_deref(), &preview.title, color, px(80.)))
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(preview.title.clone()))
                    .child(div().text_sm().text_color(palette.text_secondary).child(format!("Group · {members}")))
                    .when_some(preview.description.clone(), |d, text| d.child(div().text_sm().child(text)))
                    .child(div().text_sm().text_color(palette.text_secondary).child(if preview.approval {
                        "An admin of this group must approve your request before you can join this group. When you request to join, your name and photo will be shared with its members."
                    } else {
                        "Do you want to join this group and share your name and photo with its members?"
                    })),
            )
            .footer(footer)
    });
}

/// Pasting a group link to open it (an OS link handler can call
/// [`AppState::open_group_link`] too).
pub fn open_paste_link(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("https://signal.group/#…"));
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let url = input.read(cx).value().trim().to_string();
        let valid = signal_backend::group_admin::is_group_link(&url);
        let (open_state, open_url) = (state.clone(), url.clone());
        let footer = DialogFooter::new()
            .child(Button::new("group-link-paste-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
            .child(Button::new("group-link-paste-open").label("Open").primary().disabled(!valid).on_click(move |_, window, cx| {
                open_state.read(cx).open_group_link(open_url.clone());
                window.close_all_dialogs(cx);
            }));
        dialog
            .title("Join with a group link")
            .w(px(440.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().debug_selector(|| "group-link-paste-field".into()).child(Input::new(&input)))
                    .child(div().text_xs().text_color(palette.text_secondary).child("Paste a signal.group link someone shared with you.")),
            )
            .footer(footer)
    });
}

/// Desktop's "N groups in common" (`ConversationDetailsGroups`): groups
/// we're in with them, by title.
pub fn groups_in_common(state: &AppState, aci: &str) -> Vec<Conversation> {
    let mut groups: Vec<Conversation> = state
        .conversations
        .iter()
        .chain(state.archived.iter())
        .filter(|c| {
            c.kind == ConversationKind::Group
                && !c.left
                && c.group_state.membership == Membership::Member
                && c.members.iter().any(|m| m == aci)
        })
        .cloned()
        .collect();
    groups.sort_by_key(|c| c.title.to_lowercase());
    groups
}

/// The "groups in common" section of a 1:1 chat's settings: each opens
/// the group.
pub fn groups_in_common_section(state: &Entity<AppState>, aci: &str, palette: Palette, cx: &App) -> AnyElement {
    let groups = groups_in_common(state.read(cx), aci);
    let title = match groups.len() {
        1 => "1 group in common".to_owned(),
        n => format!("{n} groups in common"),
    };
    let rows = groups.into_iter().map(|group| {
        let (state, id) = (state.clone(), group.id.clone());
        let selector = format!("common-{}", group.id.0);
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector)
            .flex()
            .items_center()
            .gap_2()
            .p_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(palette.row_hover))
            .child(conversation_avatar(&group, px(28.)))
            .child(div().truncate().child(group.title.clone()))
            .on_click(move |_, window, cx| {
                window.close_all_dialogs(cx);
                state.update(cx, |s, cx| s.select(Some(id.clone()), cx));
            })
    });
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child(title))
        .children(rows)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, size};
    use signal_backend::{Backend, Config};

    use super::*;

    /// The demo's groups and people (made up).
    const HIKES: &str = "g:0000000000000000000000000000000000000000000000000000000000000001";
    const HOUSE: &str = "g:0000000000000000000000000000000000000000000000000000000000000003";
    const INVITED: &str = "g:0000000000000000000000000000000000000000000000000000000000000005";
    const MAYA: &str = "00000000-0000-4000-8000-00000000a11c";
    const PRIYA: &str = "00000000-0000-4000-8000-0000000000c0";
    const SAM: &str = "00000000-0000-4000-8000-0000000000e0";
    const ANA: &str = "00000000-0000-4000-8000-0000000000f0";
    const LEO: &str = "00000000-0000-4000-8000-0000000000f1";

    fn demo(cx: &mut TestAppContext) -> (VisualTestContext, Entity<AppState>, Backend, tempfile::TempDir) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        cx.update(|cx| {
            use futures::StreamExt as _;
            let state = state.clone();
            cx.spawn(async move |cx| {
                while let Some(event) = events.next().await {
                    state.update(cx, |state, cx| state.handle(event, cx));
                }
            })
            .detach();
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(1000.), px(900.)) })),
            ..Default::default()
        };
        let (window, _root) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::root::RootView::new(state, window, cx))).unwrap()
        });
        (VisualTestContext::from_window(window, cx), state, backend, dir)
    }

    fn wait(cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..400 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn found(cx: &mut VisualTestContext, id: &'static str) -> bool {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
    }

    /// Clicks a button by its id.
    fn press(cx: &mut VisualTestContext, id: &'static str) {
        for _ in 0..300 {
            if found(cx, id) {
                cx.update(|window, cx| window.click(id, cx));
                cx.run_until_parked();
                return;
            }
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("no button {id}");
    }

    fn click_at(cx: &mut VisualTestContext, what: &'static str) {
        wait(cx, what, &|cx| cx.debug_bounds(what).is_some());
        let at = cx.debug_bounds(what).unwrap().center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
    }

    fn chat(cx: &mut VisualTestContext, state: &Entity<AppState>, id: &str) -> Option<Conversation> {
        state.update(cx, |s, cx| s.reload_conversations(cx));
        cx.run_until_parked();
        state.read_with(cx, |s, _| s.conversation(&ConversationId(id.into())).cloned())
    }

    fn until(cx: &mut VisualTestContext, state: &Entity<AppState>, id: &str, what: &str, done: impl Fn(&Conversation) -> bool) {
        wait(cx, what, &|cx| chat(cx, state, id).is_some_and(|c| done(&c)));
    }

    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    #[gpui_kit::test]
    fn admins_remove_promote_and_set_permissions(cx: &mut TestAppContext) {
        let (mut cx, state, backend, _dir) = demo(cx);
        let cx = &mut cx;
        let house = ConversationId(HOUSE.into());
        until(cx, &state, HOUSE, "the house group", |c| c.members.iter().any(|m| m == ANA));

        // Leo's details: make him an admin (Desktop's confirmation first).
        cx.update(|window, cx| open_member(state.clone(), house.clone(), LEO.into(), window, cx));
        press(cx, "member-toggle-admin");
        press(cx, "ok");
        until(cx, &state, HOUSE, "Leo to be an admin", |c| c.admins.iter().any(|a| a == LEO));

        // Remove Ana: she's gone, and banned from rejoining by the link.
        cx.update(|window, cx| open_member(state.clone(), house.clone(), ANA.into(), window, cx));
        press(cx, "member-remove");
        press(cx, "ok");
        until(cx, &state, HOUSE, "Ana to be removed", |c| !c.members.iter().any(|m| m == ANA));
        assert!(chat(cx, &state, HOUSE).unwrap().group_state.banned.iter().any(|b| b == ANA));

        // Only admins may send messages, and edit the details.
        cx.update(|window, cx| open_permissions(state.clone(), house.clone(), window, cx));
        click_at(cx, "permission-send-admins");
        until(cx, &state, HOUSE, "announcement-only", |c| c.group_state.announcements_only);
        click_at(cx, "permission-edit-info-admins");
        until(cx, &state, HOUSE, "admin-only details", |c| c.group_state.info_admins_only);
        cx.update(|window, cx| window.close_all_dialogs(cx));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn the_link_requests_and_invites_are_managed(cx: &mut TestAppContext) {
        let (mut cx, state, backend, _dir) = demo(cx);
        let cx = &mut cx;
        let house = ConversationId(HOUSE.into());
        until(cx, &state, HOUSE, "the house group's link", |c| c.group_state.link.as_ref().is_some_and(|l| l.approval));
        let before = chat(cx, &state, HOUSE).unwrap().group_state.link.unwrap().url;

        cx.update(|window, cx| open_link(state.clone(), house.clone(), window, cx));
        wait(cx, "the link", &|cx| cx.debug_bounds("group-link-url").is_some());
        // Approval off, then a new link.
        press(cx, "group-link-approval");
        until(cx, &state, HOUSE, "approval off", |c| c.group_state.link.as_ref().is_some_and(|l| !l.approval));
        press(cx, "group-link-reset");
        press(cx, "ok");
        until(cx, &state, HOUSE, "a new link", |c| c.group_state.link.as_ref().is_some_and(|l| l.url != before));
        // Off.
        press(cx, "group-link-toggle");
        until(cx, &state, HOUSE, "the link off", |c| c.group_state.link.is_none());
        cx.update(|window, cx| window.close_all_dialogs(cx));

        // Priya's request, approved; Sam's invitation, revoked.
        cx.update(|window, cx| open_requests(state.clone(), house.clone(), window, cx));
        wait(cx, "Priya's request", &|cx| cx.debug_bounds(leak(format!("request-{PRIYA}"))).is_some());
        press(cx, leak(format!("approve-{PRIYA}")));
        press(cx, "ok");
        until(cx, &state, HOUSE, "Priya to join", |c| c.members.iter().any(|m| m == PRIYA) && c.group_state.requests.is_empty());
        cx.update(|window, cx| {
            window.close_all_dialogs(cx);
            open_requests(state.clone(), house.clone(), window, cx)
        });
        press(cx, leak(format!("revoke-{SAM}")));
        press(cx, "ok");
        until(cx, &state, HOUSE, "Sam's invitation to go", |c| c.group_state.invites.is_empty());
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn an_invitation_is_accepted_in_place_of_the_composer(cx: &mut TestAppContext) {
        let (mut cx, state, backend, _dir) = demo(cx);
        let cx = &mut cx;
        until(cx, &state, INVITED, "the invitation", |c| matches!(c.group_state.membership, Membership::Invited { .. }));
        // Not a group in common with Maya until we're in it.
        assert!(state.read_with(cx, |s, _| groups_in_common(s, MAYA)).iter().all(|g| g.id.0 != INVITED));
        state.update(cx, |s, cx| s.select(Some(ConversationId(INVITED.into())), cx));
        wait(cx, "the invitation actions", &|cx| cx.debug_bounds("group-invite-actions").is_some());
        press(cx, "invite-accept");
        until(cx, &state, INVITED, "us to join", |c| c.group_state.membership == Membership::Member);
        wait(cx, "the composer", &|cx| cx.debug_bounds("group-invite-actions").is_none());
        let common: Vec<String> = state.read_with(cx, |s, _| groups_in_common(s, MAYA)).into_iter().map(|g| g.id.0).collect();
        assert!(common.contains(&HIKES.to_owned()) && common.contains(&INVITED.to_owned()), "{common:?}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn members_who_arent_admins_see_the_announcement_banner(cx: &mut TestAppContext) {
        let (mut cx, state, backend, _dir) = demo(cx);
        let cx = &mut cx;
        until(cx, &state, HIKES, "the hiking group", |c| !c.members.is_empty());
        let hikes = ConversationId(HIKES.into());
        state.update(cx, |s, cx| s.select(Some(hikes.clone()), cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("announcements-only-banner").is_none());
        // Maya (an admin there) turns it on.
        state.read_with(cx, |s, _| {
            s.send(Command::SetGroupPermission { conversation: hikes.clone(), permission: Permission::SendMessages { admins_only: true } })
        });
        until(cx, &state, HIKES, "announcement-only", |c| c.group_state.announcements_only);
        wait(cx, "the banner", &|cx| cx.debug_bounds("announcements-only-banner").is_some());
        backend.shutdown();
    }

    /// A made-up link to a made-up group (`key`), whose password starts
    /// with `first` (the demo asks an admin when it's odd).
    fn link(key: u8, first: u8) -> (String, &'static str) {
        let mut password = [first; 16];
        password[1] = 0;
        let url = signal_backend::group_admin::link_url([key; 32], &password);
        (url, leak(format!("g:{}", hex::encode([key; 32]))))
    }

    #[gpui_kit::test]
    fn a_group_link_is_joined_or_asked_to_join(cx: &mut TestAppContext) {
        let (mut cx, state, backend, _dir) = demo(cx);
        let cx = &mut cx;
        until(cx, &state, HIKES, "the demo", |c| !c.members.is_empty());
        let (asking, group) = link(9, 1);
        state.read_with(cx, |s, _| s.open_group_link(asking.clone()));
        wait(cx, "the join dialog", &|cx| cx.debug_bounds("group-join-dialog").is_some());
        press(cx, "group-join");
        until(cx, &state, group, "our request", |c| matches!(c.group_state.membership, Membership::Requesting { .. }));
        assert_eq!(state.read_with(cx, |s, _| s.selected.clone()), Some(ConversationId(group.into())), "the chat opens");
        wait(cx, "the waiting notice", &|cx| cx.debug_bounds("group-request-pending").is_some());
        press(cx, "cancel-join-request");
        press(cx, "ok");
        until(cx, &state, group, "the request to go", |c| c.group_state.membership == Membership::Member);

        // A link anyone may join by: joined at once.
        let (open, joined) = link(8, 2);
        state.read_with(cx, |s, _| s.open_group_link(open.clone()));
        press(cx, "group-join");
        until(cx, &state, joined, "us to join", |c| c.group_state.membership == Membership::Member && !c.left);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn links_are_pasted_to_open_them(cx: &mut TestAppContext) {
        let (mut cx, state, backend, _dir) = demo(cx);
        let cx = &mut cx;
        cx.update(|window, cx| open_paste_link(state.clone(), window, cx));
        let (url, _) = link(7, 2);
        assert!(!found(cx, "group-join-dialog"));
        // Typed into the field (it has focus once clicked).
        click_at(cx, "group-link-paste-field");
        cx.simulate_input(&url);
        press(cx, "group-link-paste-open");
        wait(cx, "the join dialog", &|cx| cx.debug_bounds("group-join-dialog").is_some());
        backend.shutdown();
    }
}
