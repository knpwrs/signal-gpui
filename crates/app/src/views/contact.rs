//! People, as Signal Desktop shows them: the contact modal for someone in a
//! group (`ts/components/conversation/ContactModal.dom.tsx`), the About
//! screen (`AboutContactModal.dom.tsx`), the hero at the start of a chat
//! (`ConversationHero.dom.tsx`), "Profile names are not verified"
//! (`ProfileNameWarningModal`), groups in common (`SharedGroupNames`) and
//! the badge dialog (`BadgeDialog.dom.tsx`).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Context, Entity, FontWeight, SharedString, Subscription, Window, div, px};
use signal_backend::{ChatChange, Command, ContactDetails};
use signal_store::{Conversation, ConversationId, ConversationKind, DirectoryEntry};

use crate::state::{AppEvent, AppState};
use crate::views::message_request::{first_name, shared_groups, shared_groups_text};
use crate::theme::Palette;
use crate::views::common::{badge_image, badged_avatar, with_badge};

/// Whether Desktop warns that their name isn't verified (the hero's
/// `NameNotVerifiedWarning`): they have a profile name, and it's neither
/// one we gave them nor in our address book.
pub fn name_unverified(details: &ContactDetails) -> bool {
    let has_profile_name = details.profile.as_ref().is_some_and(|p| p.profile_name().is_some());
    let has_nickname = details.nickname.as_ref().is_some_and(|n| n.nickname().is_some());
    has_profile_name && !has_nickname && !details.in_system_contacts
}

/// What's known about someone, loaded from the database and kept up to
/// date while shown.
pub struct ContactInfo {
    pub service_id: String,
    /// Their name as the app shows it (nickname, address book, profile).
    pub name: String,
    pub details: Option<ContactDetails>,
    _subscription: Subscription,
}

impl ContactInfo {
    /// Loads it, and asks for their profile fresh (Desktop fetches it when
    /// they're shown).
    pub fn load(state: &Entity<AppState>, service_id: String, cx: &mut App) -> Entity<ContactInfo> {
        state.read(cx).send(Command::LoadProfile { service_id: service_id.clone() });
        let name = state
            .read(cx)
            .conversation(&ConversationId::direct(&service_id))
            .map(|c| c.title.clone())
            .unwrap_or_else(|| "Unknown".into());
        let info = cx.new(|cx| {
            let sid = service_id.clone();
            let subscription = cx.subscribe(state, move |this: &mut ContactInfo, state, event: &AppEvent, cx| match event {
                AppEvent::ProfileChanged(changed) if *changed == sid => this.reload(&state, cx),
                AppEvent::ConversationsChanged => this.reload(&state, cx),
                _ => {}
            });
            ContactInfo { service_id, name, details: None, _subscription: subscription }
        });
        info.update(cx, |info, cx| info.reload(state, cx));
        info
    }

    fn reload(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let backend = state.read(cx).backend.clone();
        let db = backend.db().clone();
        let sid = self.service_id.clone();
        let read = backend.spawn(async move {
            let name = db.display_names(std::slice::from_ref(&sid)).await.ok().and_then(|n| n.into_iter().next()).map(|(_, n)| n);
            (signal_backend::contact_details(&db, &sid).await, name)
        });
        cx.spawn(async move |this, cx| {
            let (details, name) = read.await;
            let _ = this.update(cx, |this, cx| {
                // A profile name when nothing better is known.
                let name = name.or_else(|| details.profile.as_ref().and_then(|p| p.profile_name()));
                if let Some(name) = name {
                    this.name = name;
                }
                this.details = Some(details);
                cx.notify();
            });
        })
        .detach();
    }
}

/// A row of a list: an icon and its text.
fn item(icon: IconName, label: impl IntoElement, palette: Palette) -> gpui_kit::Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_2()
        .py_2()
        .rounded_md()
        .child(Icon::new(icon).size_4().flex_none().text_color(palette.text_secondary))
        .child(div().flex_1().min_w_0().text_sm().child(label))
}

fn clickable(id: &'static str, icon: IconName, label: impl IntoElement, palette: Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    item(icon, label, palette)
        .id(id)
        .debug_selector(move || id.into())
        .cursor_pointer()
        .hover(|s| s.bg(palette.row_hover))
}

/// Desktop's "Profile names are not verified" explanation
/// (`ProfileNameWarningModal`), for a person or a group.
pub fn open_profile_name_warning(group: bool, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let (intro, points): (&str, [&str; 4]) = if group {
            (
                "Group names are chosen by members of the group.",
                [
                    "Be cautious of groups that impersonate organizations and businesses",
                    "Profile names of members in groups are not verified",
                    "Don't share personal information with people you don't know",
                    "",
                ],
            )
        } else {
            (
                "Profile names on Signal are chosen by their account holder:",
                [
                    "Signal can’t verify names and photos",
                    "Signal will never contact you for your registration code, PIN, or recovery key",
                    "Be cautious of accounts that impersonate others",
                    "Don't share personal information with people you don't know",
                ],
            )
        };
        dialog.w(px(400.)).child(
            div()
                .debug_selector(|| "profile-name-warning".into())
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_sm().child(intro))
                .children(points.into_iter().filter(|p| !p.is_empty()).map(|point| {
                    div()
                        .flex()
                        .gap_2()
                        .text_sm()
                        .child(Icon::new(IconName::Info).size_4().flex_none().text_color(palette.text_secondary))
                        .child(point)
                })),
        )
    });
}

/// Desktop's About screen for someone (or, `service_id` ours, "You").
pub fn open_about(state: Entity<AppState>, service_id: String, window: &mut Window, cx: &mut App) {
    let me = state.read(cx).our_aci() == Some(service_id.as_str());
    let info = ContactInfo::load(&state, service_id.clone(), cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let info_now = info.read(cx);
        let details = info_now.details.clone().unwrap_or_default();
        let s = state.read(cx);
        let own = s.own_profile.clone().unwrap_or_default();
        let name = if me { own.name().unwrap_or_else(|| "You".into()) } else { info_now.name.clone() };
        let chat = ConversationId::direct(&service_id);
        let conversation = s.conversation(&chat).cloned();
        let photo = crate::views::common::person_avatar(s.photo_of(&service_id), &name, crate::theme::color_for(&service_id), px(160.));
        // Desktop's "Nickname (profile name)" when a nickname shows.
        let plain = details.nickname.as_ref().filter(|n| n.nickname().is_some()).and_then(|n| n.plain_name.clone()).filter(|p| *p != name);
        let name_label = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .child(SharedString::from(name.clone()))
            .when_some(plain, |d, plain| d.child(div().text_color(palette.text_secondary).child(format!("({plain})"))));
        let profile = details.profile.clone().unwrap_or_default();
        let about = (!me && !profile.about.is_empty()).then(|| format!("{} {}", profile.about_emoji, profile.about).trim().to_owned());
        let system_name = (!me && details.in_system_contacts).then(|| first_name(&name).to_owned());
        let groups = shared_groups(s, &service_id);
        let request = conversation.as_ref().is_some_and(|c| !c.accepted);
        let note = details.nickname.as_ref().map(|n| n.note.clone()).filter(|n| !n.is_empty());
        let phone = conversation.as_ref().and_then(|c| c.phone.clone()).filter(|_| !me);
        let has_messages = conversation.as_ref().is_some_and(|c| c.last_message_at > 0);
        let blocked = conversation.as_ref().is_some_and(|c| c.blocked);
        let (edit_state, qr_state, safety_state, note_state) = (state.clone(), state.clone(), state.clone(), state.clone());
        let (safety_chat, safety_name) = (chat.clone(), name.clone());
        let note_chat = conversation.clone();
        let note_nickname = details.nickname.clone().unwrap_or_default();
        let list = div()
            .debug_selector(|| "about-contact".into())
            .flex()
            .flex_col()
            .map(|d| {
                if me {
                    d.child(clickable("about-name", IconName::User, name_label, palette).on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        crate::views::profile_editor::open(edit_state.clone(), window, cx);
                    }))
                } else {
                    d.child(item(IconName::User, name_label, palette).debug_selector(|| "about-name".into()))
                }
            })
            .when_some(about, |d, about| d.child(item(IconName::Pencil, div().debug_selector(|| "about-about".into()).child(about), palette)))
            .when(!me && details.verified, |d| {
                d.child(clickable("about-verified", IconName::BadgeCheck, "Verified", palette).on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    crate::views::safety_number::open(safety_state.clone(), safety_chat.clone(), safety_name.clone(), window, cx);
                }))
            })
            .when(!me && details.profile_sharing, |d| d.child(item(IconName::Users, "Signal Connection", palette)))
            .when_some(system_name, |d, system| {
                d.child(item(IconName::SquareUser, format!("{system} is in your system contacts"), palette))
            })
            .when(!me && name_unverified(&details), |d| {
                d.child(
                    clickable("about-names-unverified", IconName::CircleAlert, "Profile names are not verified", palette)
                        .on_click(|_, window, cx| open_profile_name_warning(false, window, cx)),
                )
            })
            .when(me && own.username.is_some(), |d| {
                d.child(clickable("about-qr", IconName::Link, "Your QR code", palette).on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    crate::views::profile_editor::open_at(qr_state.clone(), crate::views::profile_editor::Page::UsernameLink, window, cx);
                }))
            })
            .when_some(phone, |d, phone| d.child(item(IconName::Phone, phone, palette)))
            .when(!me, |d| {
                d.child(item(
                    IconName::Users,
                    div().debug_selector(|| "about-groups".into()).child(shared_groups_text(&groups)),
                    palette,
                ))
            })
            .when_some(note.zip(note_chat), |d, (note, chat)| {
                d.child(clickable("about-note", IconName::NotebookText, div().line_clamp(3).child(note), palette).on_click(
                    move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        crate::views::nickname::open_note(note_state.clone(), chat.clone(), note_nickname.clone(), window, cx);
                    },
                ))
            })
            .when(!me && blocked, |d| d.child(item(IconName::Ban, format!("{name} is blocked"), palette)))
            .when(!me && !blocked && request, |d| d.child(item(IconName::MessageSquareDot, "Pending message request", palette)))
            .when(!me && !blocked && !request && !has_messages && !details.profile_sharing, |d| {
                d.child(item(IconName::MessageCircle, format!("No direct messages with {name}"), palette))
            });
        dialog
            .title(if me { "You" } else { "About" })
            .w(px(400.))
            .child(div().flex().flex_col().gap_3().child(div().flex().justify_center().child(photo)).child(list))
    });
}

/// The group's actions in the contact modal, for someone opened from a
/// group (Desktop's `canDoGroupAdminActions` block: make or remove admin,
/// remove from the group; `group_admin::member_actions`).
pub fn group_actions(state: &Entity<AppState>, group: &Conversation, service_id: &str, name: &str, cx: &App) -> Vec<AnyElement> {
    crate::views::group_admin::member_actions(state, group, service_id, name, cx)
}

/// Desktop's contact modal for someone, from a group (`group`) or
/// elsewhere: their photo and badge, name (opening About), member label,
/// Message, Video and Voice, then Nickname, Block, View safety number and
/// any group actions.
pub fn open_contact(state: Entity<AppState>, service_id: String, group: Option<ConversationId>, window: &mut Window, cx: &mut App) {
    if state.read(cx).our_aci() == Some(service_id.as_str()) {
        return open_about(state, service_id, window, cx);
    }
    let info = ContactInfo::load(&state, service_id.clone(), cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let s = state.read(cx);
        let info_now = info.read(cx);
        let name = info_now.name.clone();
        let details = info_now.details.clone().unwrap_or_default();
        let chat = ConversationId::direct(&service_id);
        let conversation = s.conversation(&chat).cloned();
        let group = group.as_ref().and_then(|g| s.conversation(g)).cloned();
        let label = group.as_ref().and_then(|g| g.member_labels.get(&service_id)).cloned();
        let blocked = conversation.as_ref().is_some_and(|c| c.blocked);
        let in_call = s.call.is_some() || s.lobby.is_some();
        let badge = badge_image(s, &service_id, 80., cx);
        let photo = crate::views::common::person_avatar(s.photo_of(&service_id), &name, crate::theme::color_for(&service_id), px(80.));
        let entry = DirectoryEntry {
            id: chat.clone(),
            kind: ConversationKind::Direct,
            title: name.clone(),
            phone: conversation.as_ref().and_then(|c| c.phone.clone()),
        };
        // The chat with them: opened, then whatever the button does.
        let with_chat = {
            let (state, entry) = (state.clone(), entry.clone());
            move |then: Option<bool>, window: &mut Window, cx: &mut App| {
                window.close_all_dialogs(cx);
                let id = entry.id.clone();
                state.update(cx, |s, cx| {
                    s.start_conversation(entry.clone(), cx);
                    if let Some(video) = then {
                        s.open_call_lobby(id, video, cx);
                    }
                });
            }
        };
        let (message, video, voice) = (with_chat.clone(), with_chat.clone(), with_chat);
        let quick = |id: &'static str, icon: IconName, text: &'static str| {
            Button::new(id).icon(icon).label(text).outline().small()
        };
        let (about_state, about_sid) = (state.clone(), service_id.clone());
        let (badge_state, badge_sid, badge_name) = (state.clone(), service_id.clone(), name.clone());
        let first_name = details.profile.as_ref().map(|p| p.given_name.clone()).filter(|g| !g.is_empty()).unwrap_or_else(|| name.clone());
        let (nick_state, nick_chat, nick_current) =
            (state.clone(), conversation.clone(), details.nickname.clone().unwrap_or_default());
        let (block_state, block_chat, block_name) = (state.clone(), chat.clone(), name.clone());
        let (safety_state, safety_chat, safety_name) = (state.clone(), chat.clone(), name.clone());
        let extra = group.as_ref().map(|g| group_actions(&state, g, &service_id, &name, cx)).unwrap_or_default();
        let admin = group.as_ref().is_some_and(|g| g.admins.contains(&service_id));
        let body = div()
            .debug_selector(|| "contact-modal".into())
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(match badge {
                Some(badge) => div()
                    .id("contact-badge")
                    .debug_selector(|| "contact-badge".into())
                    .cursor_pointer()
                    .child(with_badge(photo, 80., Some(badge)))
                    .on_click(move |_, window, cx| open_badges(badge_state.clone(), badge_sid.clone(), badge_name.clone(), first_name.clone(), window, cx))
                    .into_any_element(),
                None => photo,
            })
            .child(
                div()
                    .id("contact-name")
                    .debug_selector(|| "contact-name".into())
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(name.clone()))
                    .when(details.in_system_contacts, |d| {
                        d.child(Icon::new(IconName::SquareUser).size_4().text_color(palette.text_secondary))
                    })
                    .child(Icon::new(IconName::ChevronRight).size_4().text_color(palette.text_secondary))
                    .on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        open_about(about_state.clone(), about_sid.clone(), window, cx);
                    }),
            )
            .when_some(label, |d, label| {
                d.child(div().text_sm().text_color(palette.text_secondary).child(match label.emoji {
                    Some(emoji) => format!("{emoji} {}", label.text),
                    None => label.text,
                }))
            })
            .when(admin, |d| d.child(div().text_xs().text_color(palette.text_secondary).child("Admin")))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(quick("contact-message", IconName::MessageCircle, "Message").on_click(move |_, window, cx| message(None, window, cx)))
                    .child(quick("contact-video", IconName::Video, "Video").disabled(in_call).on_click(move |_, window, cx| video(Some(true), window, cx)))
                    .child(quick("contact-voice", IconName::Phone, "Voice").disabled(in_call).on_click(move |_, window, cx| voice(Some(false), window, cx))),
            )
            .child(div().w_full().h(px(1.)).bg(palette.panel_border))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .when_some(nick_chat.filter(crate::views::nickname::can_have_nickname), |d, chat| {
                        d.child(clickable("contact-nickname", IconName::Pencil, "Nickname", palette).on_click(move |_, window, cx| {
                            window.close_all_dialogs(cx);
                            crate::views::nickname::open_edit(nick_state.clone(), chat.clone(), nick_current.clone(), window, cx);
                        }))
                    })
                    .map(|d| {
                        if blocked {
                            d.child(item(IconName::Ban, format!("{name} is blocked"), palette))
                        } else {
                            d.child(clickable("contact-block", IconName::Ban, "Block", palette).on_click(move |_, window, cx| {
                                confirm_block(block_state.clone(), block_chat.clone(), block_name.clone(), window, cx)
                            }))
                        }
                    })
                    .child(clickable("contact-safety-number", IconName::Lock, "View safety number", palette).on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        crate::views::safety_number::open(safety_state.clone(), safety_chat.clone(), safety_name.clone(), window, cx);
                    }))
                    .children(extra),
            );
        dialog.w(px(360.)).child(body)
    });
}

/// Desktop's "Block {title}?" from the contact modal.
fn confirm_block(state: Entity<AppState>, chat: ConversationId, name: String, window: &mut Window, cx: &mut App) {
    let title = format!("Block {name}?");
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, chat) = (state.clone(), chat.clone());
        alert
            .title(SharedString::from(title.clone()))
            .description("Blocked people won't be able to call you or send you messages.")
            .confirm()
            .ok_text("Block")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
                state.read(cx).set_chat(chat.clone(), ChatChange::Blocked(true));
                true
            })
    });
}

/// Desktop's badge dialog: each badge's image, name and description, one
/// at a time, and "Donate to Signal" for donor badges.
pub fn open_badges(state: Entity<AppState>, service_id: String, name: String, first_name: String, window: &mut Window, cx: &mut App) {
    let shown = cx.new(|_| 0usize);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let s = state.read(cx);
        let badges = s.badges.of(&service_id);
        let ix = (*shown.read(cx)).min(badges.len().saturating_sub(1));
        let Some(badge) = badges.get(ix).cloned() else { return dialog.child(div()) };
        let dark = gpui_kit::component::ActiveTheme::theme(cx).is_dark();
        let image = s.badges.image(&badge, 160., dark);
        let donor = badge.category == signal_store::BadgeCategory::Donor;
        let count = badges.len();
        let (prev, next) = (shown.clone(), shown.clone());
        dialog.w(px(400.)).child(
            div()
                .debug_selector(|| "badge-dialog".into())
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .when_some(image, |d, image| d.child(gpui_kit::img(image).size(px(160.))))
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(if donor {
                    format!("{name} supports Signal")
                } else {
                    badge.name.clone()
                }))
                .child(div().text_sm().text_center().text_color(palette.text_secondary).child(badge.describe(&first_name)))
                .when(donor, |d| {
                    d.child(Button::new("badge-donate").label("Donate to Signal").primary().on_click(|_, _, cx| {
                        cx.open_url("https://signal.org/donate/");
                    }))
                })
                .when(count > 1, |d| {
                    d.child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("badge-previous").icon(IconName::ChevronLeft).ghost().small().disabled(ix == 0).on_click(
                                move |_, _, cx| prev.update(cx, |i, cx| {
                                    *i = i.saturating_sub(1);
                                    cx.notify();
                                }),
                            ))
                            .child(div().text_sm().child(format!("{} of {count}", ix + 1)))
                            .child(Button::new("badge-next").icon(IconName::ChevronRight).ghost().small().disabled(ix + 1 >= count).on_click(
                                move |_, _, cx| next.update(cx, |i, cx| {
                                    *i += 1;
                                    cx.notify();
                                }),
                            )),
                    )
                }),
        )
    });
}

/// Desktop's hero at the start of a chat (`ConversationHero`): who it's
/// with, their name (opening About), whether the name is verified, and
/// the groups we share; or a group's description and members; or Note to
/// Self. `details` are the person's, for a 1:1 chat.
pub fn hero(state: &Entity<AppState>, conversation: &Conversation, details: Option<&ContactDetails>, cx: &App) -> AnyElement {
    let palette = Palette::get(cx);
    let s = state.read(cx);
    let avatar = match conversation.id.as_direct() {
        Some(sid) if conversation.kind == ConversationKind::Direct => badged_avatar(s, sid, &conversation.title, 72., cx),
        _ => crate::views::common::conversation_avatar(conversation, px(72.)),
    };
    let root = div()
        .debug_selector(|| "conversation-hero".into())
        .w(px(256.))
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .px_5()
        .pt_4()
        .pb_5()
        .rounded_3xl()
        .border_2()
        .border_color(palette.panel_border)
        .child(avatar);
    let title = |text: String| div().text_center().text_xl().font_weight(FontWeight::MEDIUM).child(SharedString::from(text));
    // A message request adds Desktop's request parts: "Safety tips", and
    // "Name not verified" for a group.
    let request = !conversation.accepted && conversation.kind != ConversationKind::NoteToSelf;
    let unverified_pill = |id: &'static str, group: bool| {
        div()
            .id(id)
            .debug_selector(move || id.into())
            .flex()
            .items_center()
            .gap_1()
            .px_2p5()
            .py_1()
            .rounded_full()
            .bg(crate::views::message_request::safety_tint(cx))
            .text_color(crate::views::message_request::safety_color(cx))
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(Icon::new(if group { IconName::Users } else { IconName::User }).size_4())
            .child("Name not verified")
            .on_click(move |_, window, cx| open_profile_name_warning(group, window, cx))
    };
    let body = match conversation.kind {
        ConversationKind::NoteToSelf => root
            .child(title("Note to Self".into()))
            .child(
                div()
                    .text_sm()
                    .text_center()
                    .child("You can add notes for yourself in this chat. If your account has any linked devices, new notes will be synced."),
            ),
        ConversationKind::Direct => {
            let sid = conversation.id.as_direct().unwrap_or_default().to_owned();
            // Until the details load, a request's warning follows the
            // address book, as the request bar does.
            let unverified = details.map_or(request && !conversation.system_contact, name_unverified);
            let shared = shared_groups(s, &sid);
            let groups = if request && shared.is_empty() {
                "No groups in common. Review requests carefully.".to_owned()
            } else {
                shared_groups_text(&shared)
            };
            let about = details
                .and_then(|d| d.profile.as_ref())
                .filter(|p| !p.about.is_empty())
                .map(|p| format!("{} {}", p.about_emoji, p.about).trim().to_owned());
            let about_state = state.clone();
            root.child(
                div()
                    .id("hero-title")
                    .debug_selector(|| "hero-title".into())
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .child(title(conversation.title.clone()))
                    .child(Icon::new(IconName::ChevronRight).size_4().text_color(palette.text_secondary))
                    .on_click(move |_, window, cx| open_about(about_state.clone(), sid.clone(), window, cx)),
            )
            .when_some(about, |d, about| d.child(div().text_sm().text_center().text_color(palette.text_secondary).child(about)))
            .when(unverified, |d| d.child(unverified_pill("hero-name-unverified", false)))
            .map(|d| {
                d.child(
                    div()
                        .debug_selector(|| "hero-groups".into())
                        .flex()
                        .max_w_full()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .child(Icon::new(IconName::Users).size_4().flex_none())
                        .child(div().min_w_0().text_center().child(groups)),
                )
            })
        }
        ConversationKind::Group => {
            let me = s.our_aci().unwrap_or_default();
            let names = |sid: &String| {
                s.conversation(&ConversationId::direct(sid)).map(|c| c.title.clone()).unwrap_or_else(|| "Unknown".into())
            };
            let others: Vec<String> = conversation.members.iter().filter(|m| m.as_str() != me).map(names).collect();
            let has_me = conversation.members.iter().any(|m| m == me);
            root.child(title(conversation.title.clone()))
                .when(request, |d| d.child(unverified_pill("hero-group-unverified", true)))
                .when_some(conversation.description.clone(), |d, text| {
                    d.child(div().text_sm().text_center().line_clamp(3).child(SharedString::from(text)))
                })
                .child(
                    div()
                        .debug_selector(|| "hero-members".into())
                        .flex()
                        .max_w_full()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .child(Icon::new(IconName::Users).size_4().flex_none())
                        .child(div().min_w_0().text_center().child(members_text(&others, has_me))),
                )
        }
    };
    let body = body.when(request, |d| {
        d.child(div().debug_selector(|| "safety-tips".into()).mt_1().child(
            Button::new("safety-tips")
                .label("Safety tips")
                .outline()
                .on_click(|_, window, cx| crate::views::message_request::open_safety_tips(window, cx)),
        ))
    });
    div().pt_6().pb_2().px_6().flex().justify_center().child(body).into_any_element()
}

/// Desktop's `GroupMembersNames` (`ConversationHero--group-members-*`):
/// with three or more others, we count among the "others".
pub fn members_text(others: &[String], with_me: bool) -> String {
    match (others, with_me) {
        ([], false) => "No group members".into(),
        ([], true) => "No other group members yet".into(),
        ([one], false) => one.clone(),
        ([one], true) => format!("{one} and you"),
        ([one, two], false) => format!("{one} and {two}"),
        ([one, two], true) => format!("{one}, {two}, and you"),
        ([one, two, three], false) => format!("{one}, {two}, and {three}"),
        ([one, two, three, ..], _) => {
            let n = others.len() + usize::from(with_me) - 3;
            format!("{one}, {two}, {three}, and {n} other{}", if n == 1 { "" } else { "s" })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_read_as_desktops() {
        let m = |n: usize| (1..=n).map(|i| format!("M{i}")).collect::<Vec<_>>();
        assert_eq!(members_text(&m(0), true), "No other group members yet");
        assert_eq!(members_text(&m(1), true), "M1 and you");
        assert_eq!(members_text(&m(2), false), "M1 and M2");
        assert_eq!(members_text(&m(3), false), "M1, M2, and M3");
        assert_eq!(members_text(&m(5), true), "M1, M2, M3, and 3 others");
        assert_eq!(members_text(&m(3), true), "M1, M2, M3, and 1 other");
        assert_eq!(members_text(&m(4), false), "M1, M2, M3, and 1 other");
    }

    #[test]
    fn unverified_names_are_profile_names_alone() {
        let profile = signal_store::PersonProfile { given_name: "Jo".into(), ..Default::default() };
        let mut details = ContactDetails { profile: Some(profile), ..Default::default() };
        assert!(name_unverified(&details));
        details.in_system_contacts = true;
        assert!(!name_unverified(&details), "in our address book");
        details.in_system_contacts = false;
        details.nickname = Some(signal_store::ContactNickname { given: "Joey".into(), ..Default::default() });
        assert!(!name_unverified(&details), "a nickname we gave");
        assert!(!name_unverified(&ContactDetails::default()), "no profile name");
    }
}

#[cfg(test)]
mod ui_tests {
    use gpui_kit::{TestAppContext, VisualTestContext, WindowBounds, WindowOptions, size};

    use super::*;
    use crate::views::test_support::{click_at, demo, shows, wait};

    /// The demo's Maya Chen (in our address book, with a badge and an
    /// about), Jordan Reyes (in our address book), the stranger using
    /// Maya's name in a message request, and the hiking group.
    const MAYA: &str = "00000000-0000-4000-8000-00000000a11c";
    const JORDAN: &str = "00000000-0000-4000-8000-0000000000b0";
    const STRANGER: &str = "00000000-0000-4000-8000-0000000000a5";
    const GROUP: &str = "g:0000000000000000000000000000000000000000000000000000000000000001";

    fn ready(state: &Entity<AppState>, cx: &mut VisualTestContext) {
        wait(cx, "the demo's people", &mut |cx| {
            state.read_with(cx, |s, _| {
                s.badges.preferred(MAYA).is_some()
                    && s.conversation(&ConversationId::direct(JORDAN)).is_some()
                    && s.conversation(&ConversationId::direct(STRANGER)).is_some()
                    && s.conversation(&ConversationId(GROUP.into())).is_some()
            })
        });
    }

    fn close(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.close_all_dialogs(cx));
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn the_contact_modal_and_about_show_what_desktop_shows(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        ready(&state, cx);

        // From the group: her badge opens the badge dialog.
        let open = |cx: &mut VisualTestContext| {
            let state = state.clone();
            cx.update(|window, cx| open_contact(state, MAYA.into(), Some(ConversationId(GROUP.into())), window, cx));
        };
        open(cx);
        assert!(shows(cx, "contact-modal"));
        click_at(cx, "contact-badge");
        wait(cx, "the badge dialog", &mut |cx| shows(cx, "badge-dialog"));
        close(cx);

        // Her name opens About: her about, address book and groups.
        open(cx);
        click_at(cx, "contact-name");
        wait(cx, "About", &mut |cx| shows(cx, "about-contact") && shows(cx, "about-about"));
        assert!(!shows(cx, "about-names-unverified"), "she's in our address book");
        let details = futures::executor::block_on({
            let db = backend.db().clone();
            backend.spawn(async move { signal_backend::contact_details(&db, MAYA).await })
        });
        let groups = state.read_with(cx, |s, _| shared_groups(s, MAYA));
        assert!(groups.iter().any(|g| g == "Saturday hikes 🥾"), "{groups:?}");
        assert_eq!(details.profile.unwrap().about, "Planning Saturday's route");
        close(cx);

        // The stranger's name is only the one they chose, and their
        // request is pending.
        let s = state.clone();
        cx.update(|window, cx| open_about(s, STRANGER.into(), window, cx));
        wait(cx, "the warning", &mut |cx| shows(cx, "about-names-unverified"));
        click_at(cx, "about-names-unverified");
        wait(cx, "the explanation", &mut |cx| shows(cx, "profile-name-warning"));
        close(cx);

        // Ours: "You", opening the profile editor.
        let (s, me) = (state.clone(), state.read_with(cx, |s, _| s.our_aci().unwrap().to_owned()));
        cx.update(|window, cx| open_about(s, me, window, cx));
        click_at(cx, "about-name");
        wait(cx, "the profile editor", &mut |cx| shows(cx, "profile-editor"));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn chats_start_with_desktops_hero(cx: &mut TestAppContext) {
        let (backend, state, mut sidebar_cx, _dir) = demo(cx);
        ready(&state, &mut sidebar_cx);
        let open_chat = |cx: &mut TestAppContext, id: ConversationId| {
            let state = state.clone();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(900.), px(4000.)) })),
                ..Default::default()
            };
            let (window, _) = cx
                .update(|cx| {
                    gpui_kit::open_window(options, cx, |window, cx| {
                        let picker = cx.new(|cx| crate::views::emoji_picker::EmojiPicker::new(state.clone(), window, cx));
                        cx.new(|cx| crate::views::conversation::ConversationView::new(state, picker, id, window, cx))
                    })
                })
                .unwrap();
            VisualTestContext::from_window(window, cx)
        };
        // Jordan is in our address book, and we share the hiking group.
        let mut jordan = open_chat(cx, ConversationId::direct(JORDAN));
        wait(&mut jordan, "Jordan's hero", &mut |cx| shows(cx, "conversation-hero") && shows(cx, "hero-groups"));
        assert!(!shows(&mut jordan, "hero-name-unverified"));
        assert!(!shows(&mut jordan, "safety-tips"), "not a request");
        // The stranger's request: not verified, with Desktop's safety tips.
        let mut stranger = open_chat(cx, ConversationId::direct(STRANGER));
        wait(&mut stranger, "the request's hero", &mut |cx| shows(cx, "hero-name-unverified") && shows(cx, "safety-tips"));
        // Maya is in our address book: no warning.
        let mut maya = open_chat(cx, ConversationId::direct(MAYA));
        wait(&mut maya, "Maya's hero", &mut |cx| shows(cx, "hero-groups"));
        assert!(!shows(&mut maya, "hero-name-unverified"));
        // A group's names its members.
        let mut group = open_chat(cx, ConversationId(GROUP.into()));
        wait(&mut group, "the group's hero", &mut |cx| shows(cx, "hero-members"));
        // Long names wrap inside the card rather than running past it.
        let card = group.debug_bounds("conversation-hero").unwrap();
        let members = group.debug_bounds("hero-members").unwrap();
        assert!(members.left() >= card.left() && members.right() <= card.right(), "{members:?} outside {card:?}");
        backend.shutdown();
    }
}
