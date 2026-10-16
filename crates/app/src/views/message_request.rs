//! Message requests, as Signal Desktop shows them: the bar in place of the
//! composer (`ts/components/conversation/MessageRequestActions.dom.tsx`)
//! and its confirmations (`MessageRequestActionsConfirmation.dom.tsx`), the
//! hero at the start of a request (`ConversationHero.dom.tsx`), the safety
//! tips (`ts/components/SafetyTipsModal.dom.tsx`), and the name collision
//! warnings and their review (`ConversationHeader.dom.tsx`'s
//! `ConversationSubheader`, `ContactSpoofingReviewDialog.dom.tsx`). Wording
//! is Desktop's English.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{Icon, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, HighlightStyle, Hsla, SharedString, StyledText, Window, div, px};
use signal_backend::Command;
use signal_store::{Conversation, ConversationId, ConversationKind, MessageRequestResponse as Answer};

use crate::state::AppState;
use crate::theme::Palette;
use crate::views::common::conversation_avatar;

/// Desktop's `label-safety` color (`ts/axo/_tailwind-theme/colors.css`).
pub fn safety_color(cx: &App) -> Hsla {
    let dark = gpui_kit::component::ActiveTheme::theme(cx).is_dark();
    Hsla::from(gpui_kit::rgb(if dark { 0xeb977d } else { 0xb94b29 }))
}

/// Desktop's `fill-safety-tint`.
fn safety_tint(cx: &App) -> Hsla {
    let dark = gpui_kit::component::ActiveTheme::theme(cx).is_dark();
    let (rgb, a) = if dark { (0xeb977d, 0.16) } else { (0xb95929, 0.12) };
    Hsla { a, ..Hsla::from(gpui_kit::rgb(rgb)) }
}

/// The name Desktop's `ContactName` shows with `preferFirstName`: here, the
/// title's first word.
pub fn first_name(title: &str) -> &str {
    title.split_whitespace().next().unwrap_or(title)
}

/// A chat as Desktop's confirmations name it: a person's first name, a
/// group's whole title.
fn short_title(chat: &Conversation) -> &str {
    if chat.kind == ConversationKind::Group { &chat.title } else { first_name(&chat.title) }
}

/// Desktop's `SharedGroupNames`.
pub fn shared_groups_text(groups: &[String]) -> String {
    match groups {
        [] => "No groups in common".into(),
        [one] => format!("Member of {one}"),
        [a, b] => format!("Member of {a} and {b}"),
        [a, b, c] => format!("Member of {a}, {b}, and {c}"),
        [a, b, c, _] => format!("Member of {a}, {b}, {c} and one more"),
        [a, b, c, rest @ ..] => format!("Member of {a}, {b}, {c} and {} more", rest.len()),
    }
}

/// The groups we're in with someone (by their ACI), by title.
pub fn shared_groups(state: &AppState, aci: &str) -> Vec<String> {
    state
        .conversations
        .iter()
        .chain(&state.archived)
        .filter(|c| c.kind == ConversationKind::Group && !c.left && !c.invited() && c.members.iter().any(|m| m == aci))
        .map(|c| c.title.clone())
        .collect()
}

/// Who added or invited us to a group, by name, as the database knows them.
pub fn added_by_name(state: &AppState, chat: &Conversation, names: &HashMap<String, String>) -> Option<String> {
    let aci = chat.inviter()?;
    state.conversation(&ConversationId::direct(aci)).map(|c| c.title.clone()).or_else(|| names.get(aci).cloned())
}

/// Whether the bar takes the composer's place (Desktop's `CompositionArea`:
/// blocked, or a request not yet accepted).
pub fn shows_bar(chat: &Conversation) -> bool {
    !chat.id.is_signal() && chat.kind != ConversationKind::NoteToSelf && (chat.blocked || !chat.accepted)
}

/// What a button does.
type Run = Rc<dyn Fn(&mut Window, &mut App)>;

/// One button of a confirmation.
struct Choice {
    id: &'static str,
    label: String,
    danger: bool,
    run: Run,
}

/// A dialog with Desktop's Cancel and its actions, each closing it.
fn confirm(window: &mut Window, cx: &mut App, title: Option<String>, body: Rc<dyn Fn() -> AnyElement>, choices: Vec<Choice>) {
    let choices = Rc::new(choices);
    window.open_dialog(cx, move |dialog, _, _| {
        let mut footer = DialogFooter::new()
            .child(div().debug_selector(|| "mr-cancel".into()).child(Button::new("mr-cancel").label("Cancel").outline().on_click(
                |_, window, cx| window.close_all_dialogs(cx),
            )));
        for choice in choices.iter() {
            let run = choice.run.clone();
            let id = choice.id;
            let button = Button::new(id).label(choice.label.clone()).on_click(move |_, window, cx| {
                window.close_all_dialogs(cx);
                run(window, cx);
            });
            let button = if choice.danger { button.danger() } else { button.primary() };
            footer = footer.child(div().debug_selector(move || id.into()).child(button));
        }
        let dialog = match &title {
            Some(title) => dialog.title(title.clone()),
            None => dialog,
        };
        dialog.w(px(440.)).child(div().text_sm().child(body())).footer(footer)
    });
}

fn text_body(text: impl Into<SharedString>) -> Rc<dyn Fn() -> AnyElement> {
    let text: SharedString = text.into();
    Rc::new(move || div().child(text.clone()).into_any_element())
}

fn answer(state: &Entity<AppState>, chat: &ConversationId, answer: Answer) -> Run {
    let (state, chat) = (state.clone(), chat.clone());
    Rc::new(move |_, cx| state.read(cx).send(Command::AnswerMessageRequest { conversation: chat.clone(), answer }))
}

/// Desktop's `blocking` confirmation.
pub fn confirm_block(state: &Entity<AppState>, chat: &Conversation, window: &mut Window, cx: &mut App) {
    let group = chat.kind == ConversationKind::Group;
    let name = short_title(chat);
    let (title, body) = if group {
        (
            format!("Block and Leave {name}?"),
            "You will no longer receive messages or updates from this group and members won't be able to add you to this group again.",
        )
    } else {
        (format!("Block {name}?"), "Blocked people won't be able to call you or send you messages.")
    };
    let block = Choice { id: "mr-confirm-block", label: "Block".into(), danger: true, run: answer(state, &chat.id, Answer::Block) };
    confirm(window, cx, Some(title), text_body(body), vec![block]);
}

/// Desktop's `reportingAndMaybeBlocking` confirmation: Report and Block
/// (unless already blocked), or Report Spam.
pub fn confirm_report(state: &Entity<AppState>, chat: &Conversation, added_by: Option<String>, window: &mut Window, cx: &mut App) {
    let body = match (chat.kind == ConversationKind::Group, added_by) {
        (false, _) => "Signal will be notified that this person may be sending spam. Signal can’t see the content of any chats.".to_owned(),
        (true, None) => "Signal will be notified that the person who invited you to this group may be sending spam. Signal can’t see the content of any chats.".to_owned(),
        (true, Some(name)) => format!(
            "Signal will be notified that {}, who invited you to this group, may be sending spam. Signal can’t see the content of any chats.",
            first_name(&name)
        ),
    };
    let mut choices = Vec::new();
    if !chat.blocked {
        choices.push(Choice {
            id: "mr-confirm-report-block",
            label: "Report and Block".into(),
            danger: true,
            run: answer(state, &chat.id, Answer::BlockAndSpam),
        });
    }
    choices.push(Choice { id: "mr-confirm-report", label: "Report Spam".into(), danger: true, run: answer(state, &chat.id, Answer::Spam) });
    confirm(window, cx, Some("Report as spam?".into()), text_body(body), choices);
}

/// Desktop's `unblocking` confirmation, which accepts the request.
pub fn confirm_unblock(state: &Entity<AppState>, chat: &Conversation, window: &mut Window, cx: &mut App) {
    let body = if chat.kind == ConversationKind::Group {
        "Group members will be able to add you to this group again."
    } else {
        "You will be able to message and call each other."
    };
    let unblock = Choice { id: "mr-confirm-unblock", label: "Unblock".into(), danger: false, run: answer(state, &chat.id, Answer::Accept) };
    confirm(window, cx, Some(format!("Unblock {}?", short_title(chat))), text_body(body), vec![unblock]);
}

/// Desktop's `deleting` confirmation.
pub fn confirm_delete(state: &Entity<AppState>, chat: &Conversation, window: &mut Window, cx: &mut App) {
    let (title, body, label) = if chat.kind == ConversationKind::Group {
        (
            format!("Delete and Leave {}?", short_title(chat)),
            "You will leave this group, and it will be deleted from all your devices.",
            "Delete and Leave",
        )
    } else {
        ("Delete chat?".to_owned(), "This chat will be deleted from all of your devices.", "Delete")
    };
    let delete = Choice { id: "mr-confirm-delete", label: label.into(), danger: true, run: answer(state, &chat.id, Answer::Delete) };
    confirm(window, cx, Some(title), text_body(body), vec![delete]);
}

/// Desktop's `accepting` confirmation, with its bold "Signal will never".
pub fn confirm_accept(state: &Entity<AppState>, chat: &Conversation, window: &mut Window, cx: &mut App) {
    const TEXT: &str = "Only accept requests from people you trust. Signal will never message you for a registration code, PIN, or recovery key.";
    let bold = TEXT.find("Signal will never").map(|start| start..start + "Signal will never".len()).unwrap_or(0..0);
    let body: Rc<dyn Fn() -> AnyElement> = Rc::new(move || {
        StyledText::new(TEXT)
            .with_highlights([(bold.clone(), HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() })])
            .into_any_element()
    });
    let accept = Choice { id: "mr-confirm-accept", label: "Accept".into(), danger: false, run: answer(state, &chat.id, Answer::Accept) };
    confirm(window, cx, Some("Accept request?".into()), body, vec![accept]);
}

/// Desktop's `acceptedOptions`, from "Block or Report..." on the
/// "You accepted" notice.
pub fn accepted_options(state: &Entity<AppState>, chat: &Conversation, added_by: Option<String>, window: &mut Window, cx: &mut App) {
    let body = format!(
        "You accepted a message request from {}. If this was a mistake, you can choose an action below.",
        first_name(&chat.title)
    );
    let report = {
        let (state, chat, added_by) = (state.clone(), chat.clone(), added_by.clone());
        Rc::new(move |window: &mut Window, cx: &mut App| confirm_report(&state, &chat, added_by.clone(), window, cx))
    };
    let block = {
        let (state, chat) = (state.clone(), chat.clone());
        Rc::new(move |window: &mut Window, cx: &mut App| confirm_block(&state, &chat, window, cx))
    };
    let choices = vec![
        Choice { id: "mr-options-report", label: "Report...".into(), danger: true, run: report },
        Choice { id: "mr-options-block", label: "Block".into(), danger: true, run: block },
    ];
    confirm(window, cx, None, text_body(body), choices);
}

/// The bar's request: Desktop's `MessageRequests--message-*`.
pub fn request_text(chat: &Conversation) -> String {
    let name = first_name(&chat.title);
    match (chat.kind == ConversationKind::Group, chat.blocked) {
        (false, false) => format!(
            "Let {name} message you and share your name and photo with them? They won’t know you’ve seen their messages until you accept."
        ),
        (false, true) => format!(
            "Let {name} message you and share your name and photo with them? You won't receive any messages until you unblock them."
        ),
        (true, false) => {
            "Join this group and share your name and photo with its members? They won’t know you’ve seen their messages until you accept."
                .into()
        }
        (true, true) => {
            "Unblock this group and share your name and photo with its members? You won't receive any messages until you unblock them."
                .into()
        }
    }
}

/// Desktop's message request bar, in place of the composer.
pub fn render_bar(
    chat: &Conversation,
    state: &Entity<AppState>,
    added_by: Option<String>,
    shared_groups: usize,
    palette: Palette,
    cx: &App,
) -> AnyElement {
    let button = |id: &'static str, label: &'static str, danger: bool, run: Run| {
        let button = Button::new(id).label(label).on_click(move |_, window, cx| run(window, cx));
        div().debug_selector(move || id.into()).child(if danger { button.danger() } else { button.outline() })
    };
    let open = |f: fn(&Entity<AppState>, &Conversation, &mut Window, &mut App)| -> Run {
        let (state, chat) = (state.clone(), chat.clone());
        Rc::new(move |window, cx| f(&state, &chat, window, cx))
    };
    let report: Run = {
        let (state, chat, added_by) = (state.clone(), chat.clone(), added_by.clone());
        Rc::new(move |window, cx| confirm_report(&state, &chat, added_by.clone(), window, cx))
    };
    // Desktop accepts at once when you share more than one group with them.
    let accept: Run = if chat.kind == ConversationKind::Direct && shared_groups > 1 {
        answer(state, &chat.id, Answer::Accept)
    } else {
        open(confirm_accept)
    };
    div()
        .id("message-request")
        .debug_selector(|| "message-request".into())
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .px_4()
        .pt_2()
        .pb_3()
        .border_t_1()
        .border_color(palette.panel_border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .text_sm()
                .text_color(safety_color(cx))
                .child(Icon::new(IconName::TriangleAlert).size_4())
                .child("Review requests carefully"),
        )
        .child(div().text_sm().text_center().text_color(palette.text_secondary).child(request_text(chat)))
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_center()
                .gap_2()
                .when(!chat.blocked, |d| d.child(button("mr-block", "Block", true, open(confirm_block))))
                .when(chat.reported || chat.blocked, |d| d.child(button("mr-delete", "Delete", true, open(confirm_delete))))
                .when(!chat.reported, |d| d.child(button("mr-report", "Report...", true, report)))
                .when(chat.blocked, |d| d.child(button("mr-unblock", "Unblock", false, open(confirm_unblock))))
                .when(!chat.blocked, |d| d.child(button("mr-accept", "Accept", false, accept))),
        )
        .into_any_element()
}

/// Desktop's `ConversationHero` at the start of a request: who it is,
/// "Name not verified" (Desktop's `NameNotVerifiedWarning`), the groups we
/// share (or a group's members), and "Safety tips".
pub fn render_hero(chat: &Conversation, shared: &[String], members: &[String], palette: Palette, cx: &App) -> AnyElement {
    let group = chat.kind == ConversationKind::Group;
    // Desktop: a 1:1 name is verified when it's our address book's.
    let unverified = group || !chat.system_contact;
    let line = if group {
        match members.len() {
            0 => "No group members".to_owned(),
            n => format!("{} {}", n, if n == 1 { "member" } else { "members" }),
        }
    } else if shared.is_empty() {
        "No groups in common. Review requests carefully.".to_owned()
    } else {
        shared_groups_text(shared)
    };
    div().debug_selector(|| "mr-hero".into()).pt_6().pb_2().px_6().flex().justify_center().child(
        div()
            .w(px(256.))
            .px_5()
            .pb_5()
            .pt_4()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .rounded_xl()
            .border_2()
            .border_color(palette.panel_border)
            .child(conversation_avatar(chat, px(72.)))
            .child(div().text_xl().font_weight(FontWeight::MEDIUM).text_center().child(chat.title.clone()))
            .when(unverified, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2p5()
                        .py_1()
                        .rounded_full()
                        .bg(safety_tint(cx))
                        .text_color(safety_color(cx))
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(Icon::new(if group { IconName::Users } else { IconName::User }).size_4())
                        .child("Name not verified"),
                )
            })
            .when_some(chat.description.clone().filter(|_| group), |d, description| {
                d.child(div().text_sm().text_center().child(description))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .text_center()
                    .child(Icon::new(IconName::Users).size_4())
                    .child(line),
            )
            .child(
                div().debug_selector(|| "safety-tips".into()).mt_1().child(
                    Button::new("safety-tips").label("Safety tips").outline().on_click(|_, window, cx| open_safety_tips(window, cx)),
                ),
            ),
    )
    .into_any_element()
}

/// Desktop's safety tips: the summary's three, then "View more".
const SUMMARY_TIPS: [(IconName, &str, &str); 3] = [
    (
        IconName::MessageCircle,
        "Don’t respond to chats from Signal",
        "Signal will never message you for your registration code, PIN, or recovery key. Never respond to a chat pretending to be Signal.",
    ),
    (
        IconName::User,
        "Review names and photos",
        "Look out for the “Name not verified” notice. Everyone sets their own profile name in Signal.",
    ),
    (
        IconName::Hand,
        "Look out for scams",
        "Avoid vague messages that try to get you to reply. Be aware of financial tips and suspicious web links.",
    ),
];

/// "View more": one tip a page.
const DETAIL_TIPS: [(&str, &str); 6] = [
    (
        "Don’t respond to chats from Signal",
        "Signal will never message you for your registration code, PIN, or recover key. Don't reply to chats pretending to be Signal or Signal Support. Bad actors set up fake names to try to take over your account.",
    ),
    (
        "Review names and photos",
        "Look out for the “Name not verified” notice. Everyone sets their own profile name and photo in Signal. If you’re unsure who a new request is from it’s safer to ignore it.",
    ),
    (
        "Vague or irrelevant messages",
        "Spammers often start with a simple message like “Hi” to draw you in. If you respond they may engage you further.",
    ),
    (
        "Messages with web links",
        "Be careful of messages from people you don’t know that have links to websites. Never visit links from people you don’t trust.",
    ),
    (
        "Crypto or money scams",
        "If someone you don’t know messages about cryptocurrency (like Bitcoin) or a financial opportunity, be careful—it’s likely spam.",
    ),
    (
        "Fake businesses and institutions",
        "Be careful of businesses or government agencies contacting you. Messages involving tax agencies, couriers, and more can be spam.",
    ),
];

/// Which page of the safety tips shows: the summary, or a tip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TipsPage {
    Summary,
    Tip(usize),
}

/// Desktop's `SafetyTipsModal`.
pub fn open_safety_tips(window: &mut Window, cx: &mut App) {
    let page = cx.new(|_| TipsPage::Summary);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let current = *page.read(cx);
        let go = |to: TipsPage| {
            let page = page.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                page.update(cx, |p, _| *p = to);
                window.refresh();
            }
        };
        let dialog = dialog.w(px(480.)).title("Safety tips");
        match current {
            TipsPage::Summary => dialog.child(
                div()
                    .debug_selector(|| "safety-tips-summary".into())
                    .flex()
                    .flex_col()
                    .gap_5()
                    .py_2()
                    .children(SUMMARY_TIPS.iter().map(|(icon, title, body)| {
                        div()
                            .flex()
                            .items_start()
                            .gap_4()
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(44.))
                                    .rounded_full()
                                    .bg(palette.row_hover)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(Icon::new(*icon).size_5()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(*title))
                                    .child(div().text_sm().text_color(palette.text_secondary).child(*body)),
                            )
                    }))
                    .child(
                        div().debug_selector(|| "safety-tips-more".into()).child(
                            Button::new("safety-tips-more").label("View more").outline().w_full().on_click(go(TipsPage::Tip(0))),
                        ),
                    ),
            ),
            TipsPage::Tip(ix) => {
                let (title, body) = DETAIL_TIPS[ix.min(DETAIL_TIPS.len() - 1)];
                dialog.child(
                    div()
                        .debug_selector(move || format!("safety-tip-{ix}"))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .py_2()
                        .px_6()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(div().text_sm().text_color(palette.text_secondary).child(body))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .pt_2()
                                .child(div().debug_selector(|| "safety-tips-previous".into()).when(ix > 0, |d| {
                                    d.child(
                                        Button::new("safety-tips-previous")
                                            .icon(IconName::ChevronLeft)
                                            .outline()
                                            .tooltip("Previous")
                                            .on_click(go(TipsPage::Tip(ix - 1))),
                                    )
                                }))
                                // Desktop's dots, one a tip.
                                .child(div().flex().gap_1p5().children((0..DETAIL_TIPS.len()).map(|dot| {
                                    div()
                                        .id(("safety-tips-dot", dot))
                                        .size(px(8.))
                                        .rounded_full()
                                        .cursor_pointer()
                                        .bg(if dot == ix { palette.text } else { palette.panel_border })
                                        .on_click(go(TipsPage::Tip(dot)))
                                })))
                                .child(div().debug_selector(|| "safety-tips-next".into()).when(ix + 1 < DETAIL_TIPS.len(), |d| {
                                    d.child(
                                        Button::new("safety-tips-next")
                                            .icon(IconName::ChevronRight)
                                            .outline()
                                            .tooltip("Next")
                                            .on_click(go(TipsPage::Tip(ix + 1))),
                                    )
                                })),
                        ),
                )
            }
        }
    });
}

// ------------------------------------------------------------ name collisions

/// Desktop's `getSafeConversationWithSameTitle`: for a 1:1 request (not
/// blocked), an accepted 1:1 chat with the same title.
pub fn same_name_contact<'a>(chat: &Conversation, all: impl IntoIterator<Item = &'a Conversation>) -> Option<&'a Conversation> {
    if chat.kind != ConversationKind::Direct || chat.accepted || chat.blocked {
        return None;
    }
    all.into_iter().find(|other| {
        other.id != chat.id && other.kind == ConversationKind::Direct && other.accepted && other.title == chat.title
    })
}

/// Desktop's `getCollisionsFromMemberships`: a group's members (not us,
/// name known) sharing a title, by title, unless all of them are in our
/// address book.
pub fn name_collisions(members: &[(String, String)], me: &str, system: &HashSet<String>) -> BTreeMap<String, Vec<String>> {
    let mut by_title: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (aci, name) in members.iter().filter(|(aci, name)| aci != me && !name.trim().is_empty()) {
        by_title.entry(name.clone()).or_default().push(aci.clone());
    }
    by_title.retain(|_, acis| acis.len() >= 2 && !acis.iter().all(|a| system.contains(a)));
    by_title
}

/// Desktop's `hasUnacknowledgedCollisions`: someone colliding whom the
/// warning we closed didn't name.
pub fn has_unacknowledged(previous: &BTreeMap<String, Vec<String>>, current: &BTreeMap<String, Vec<String>>) -> bool {
    current.iter().any(|(title, ids)| {
        let seen = previous.get(title);
        ids.iter().any(|id| !seen.is_some_and(|s| s.contains(id)))
    })
}

/// Settings key: the collisions we closed a group's warning on (Desktop's
/// `acknowledgedGroupNameCollisions`), JSON title to ACIs.
pub fn acknowledged_key(chat: &ConversationId) -> String {
    format!("name_collisions.{}", chat.0)
}

/// A collision warning under the header (Desktop's `TimelineWarning`): the
/// text, its link, and close.
pub fn render_warning(
    text: String,
    link: &'static str,
    on_review: impl Fn(&mut Window, &mut App) + 'static,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
    palette: Palette,
) -> AnyElement {
    div()
        .id("spoofing-warning")
        .debug_selector(|| "spoofing-warning".into())
        .flex_none()
        .flex()
        .items_center()
        .gap_2()
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(palette.panel_border)
        .bg(palette.sidebar)
        .text_sm()
        .child(Icon::new(IconName::Info).size_4().text_color(palette.text_secondary))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_wrap()
                .gap_x_1()
                .child(text)
                .child(
                    div()
                        .id("spoofing-review")
                        .debug_selector(|| "spoofing-review".into())
                        .cursor_pointer()
                        .text_color(palette.accent)
                        .font_weight(FontWeight::SEMIBOLD)
                        .on_click(move |_, window, cx| on_review(window, cx))
                        .child(link),
                ),
        )
        .child(
            div().debug_selector(|| "spoofing-close".into()).child(
                Button::new("spoofing-close").ghost().icon(IconName::X).tooltip("Close").on_click(move |_, window, cx| on_close(window, cx)),
            ),
        )
        .into_any_element()
}

/// The 1:1 warning's text (Desktop's `ContactSpoofing__same-name--link`).
pub const SAME_NAME_TEXT: &str = "Review requests carefully. Signal found another contact with the same name.";

/// A group warning's text (Desktop's `ContactSpoofing__same-name-in-group--link`
/// and `__same-names-in-group--link`).
pub fn group_warning_text(collisions: &BTreeMap<String, Vec<String>>) -> String {
    let people: usize = collisions.values().map(Vec::len).sum();
    if collisions.len() == 1 {
        format!("{people} group {} the same name.", if people == 1 { "member has" } else { "members have" })
    } else {
        let n = collisions.len();
        format!("{n} name {} found in this group.", if n == 1 { "conflict was" } else { "conflicts were" })
    }
}

/// One person in a review (Desktop's `ContactSpoofingReviewDialogPerson`).
#[derive(Clone)]
pub struct ReviewPerson {
    pub aci: String,
    pub name: String,
    pub phone: Option<String>,
    pub shared_groups: Vec<String>,
    /// Their chat, when we have one.
    pub chat: Option<Conversation>,
}

impl ReviewPerson {
    pub fn of(state: &AppState, aci: &str, name: String) -> Self {
        let chat = state.conversation(&ConversationId::direct(aci)).cloned();
        ReviewPerson {
            aci: aci.to_owned(),
            name: chat.as_ref().map(|c| c.title.clone()).unwrap_or(name),
            phone: chat.as_ref().and_then(|c| c.phone.clone()),
            shared_groups: shared_groups(state, aci),
            chat,
        }
    }

    /// Their chat, or one standing in for it (a person we have no chat
    /// with: the backend makes one when it's answered).
    fn chat_or_new(&self) -> Conversation {
        self.chat.clone().unwrap_or_else(|| Conversation {
            id: ConversationId::direct(&self.aci),
            kind: ConversationKind::Direct,
            title: self.name.clone(),
            phone: self.phone.clone(),
            color: signal_store::avatar_color(&self.aci),
            members: Vec::new(),
            last_message_preview: None,
            last_message_at: 0,
            unread_count: 0,
            pinned: false,
            muted: false,
            muted_until: 0,
            archived: false,
            marked_unread: false,
            blocked: false,
            draft: None,
            expire_timer: 0,
            expire_timer_version: 1,
            can_edit_info: true,
            left: false,
            description: None,
            admins: Vec::new(),
            member_labels: Default::default(),
            can_label: false,
            can_add_members: false,
            ended: false,
            group_state: Default::default(),
            avatar_path: None,
            last_outgoing_status: None,
            unread_poll_votes: false,
            accepted: true,
            message_request: Default::default(),
            reported: false,
            added_by: None,
            system_contact: false,
        })
    }

    fn render(&self, palette: Palette, buttons: Option<AnyElement>) -> AnyElement {
        let chat = self.chat_or_new();
        let groups = if self.shared_groups.is_empty() {
            "No other groups in common".to_owned()
        } else {
            shared_groups_text(&self.shared_groups)
        };
        div()
            .flex()
            .items_start()
            .gap_3()
            .py_2()
            .child(conversation_avatar(&chat, px(48.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(self.name.clone()))
                    .when_some(self.phone.clone(), |d, phone| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_sm()
                                .text_color(palette.text_secondary)
                                .child(Icon::new(IconName::Phone).size_3p5())
                                .child(phone),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .text_color(palette.text_secondary)
                            .child(Icon::new(IconName::Users).size_3p5())
                            .child(groups),
                    )
                    .children(buttons),
            )
            .into_any_element()
    }
}

/// Desktop's direct review: the request (Delete, Block) and our contact.
pub fn open_direct_review(state: Entity<AppState>, request: ReviewPerson, contact: ReviewPerson, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let chat = request.chat_or_new();
        let (delete_state, delete_chat) = (state.clone(), chat.clone());
        let (block_state, block_chat) = (state.clone(), chat.clone());
        let buttons = div()
            .flex()
            .gap_2()
            .pt_1()
            .child(div().debug_selector(|| "review-delete".into()).child(Button::new("review-delete").label("Delete").danger().on_click(
                move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    confirm_delete(&delete_state, &delete_chat, window, cx);
                },
            )))
            .child(div().debug_selector(|| "review-block".into()).child(Button::new("review-block").label("Block").danger().on_click(
                move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    confirm_block(&block_state, &block_chat, window, cx);
                },
            )))
            .into_any_element();
        dialog
            .title("Review request")
            .w(px(460.))
            .child(
                div()
                    .debug_selector(|| "review-request".into())
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.text_secondary)
                            .child("If you're not sure who the request is from, review the contacts below and take action."),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Request"))
                    .child(request.render(palette, Some(buttons)))
                    .child(div().h(px(1.)).bg(palette.panel_border))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Your contact"))
                    .child(contact.render(palette, None)),
            )
    });
}

/// Desktop's group review ("Review members"): each colliding member, with
/// "Remove from group" when we're an admin (group administration's
/// `RemoveFromGroup`, after Desktop's `RemoveGroupMemberConfirmationDialog`),
/// else Unblock when blocked, else Block unless they're in our address book.
pub fn open_group_review(
    state: Entity<AppState>,
    // The group, when we're its admin.
    admin_of: Option<ConversationId>,
    collisions: BTreeMap<String, Vec<String>>,
    people: Vec<ReviewPerson>,
    system: HashSet<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let total: usize = collisions.values().map(Vec::len).sum();
    let description = if collisions.len() > 1 {
        format!(
            "{} name {} found in this group. Review the members below or choose to take action.",
            collisions.len(),
            "conflicts were"
        )
    } else {
        format!(
            "{total} group {} have the same name, review the members below or choose to take action.",
            if total == 1 { "member" } else { "members" }
        )
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let rows = people.iter().enumerate().map(|(ix, person)| {
            let chat = person.chat_or_new();
            let button = if let Some(group) = admin_of.clone() {
                let (state, aci, name) = (state.clone(), person.aci.clone(), person.name.clone());
                Some(
                    Button::new(("review-remove", ix))
                        .label("Remove from group")
                        .danger()
                        .on_click(move |_, window, cx| {
                            window.close_all_dialogs(cx);
                            let (state, group, aci) = (state.clone(), group.clone(), aci.clone());
                            let remove: Run = Rc::new(move |_, cx| {
                                state.read(cx).send(Command::RemoveFromGroup { conversation: group.clone(), service_id: aci.clone() })
                            });
                            let choice = Choice { id: "review-confirm-remove", label: "Remove".into(), danger: true, run: remove };
                            confirm(window, cx, None, text_body(format!("Remove \"{name}\" from the group?")), vec![choice]);
                        })
                        .into_any_element(),
                )
            } else if chat.blocked {
                let (state, chat) = (state.clone(), chat.clone());
                Some(
                    Button::new(("review-unblock", ix))
                        .label("Unblock")
                        .outline()
                        .on_click(move |_, _, cx| {
                            state.read(cx).send(Command::AnswerMessageRequest { conversation: chat.id.clone(), answer: Answer::Accept })
                        })
                        .into_any_element(),
                )
            } else if !system.contains(&person.aci) {
                let (state, chat) = (state.clone(), chat.clone());
                Some(
                    Button::new(("review-block", ix))
                        .label("Block")
                        .danger()
                        .on_click(move |_, window, cx| {
                            window.close_all_dialogs(cx);
                            confirm_block(&state, &chat, window, cx);
                        })
                        .into_any_element(),
                )
            } else {
                None
            };
            let buttons = button.map(|b| div().pt_1().child(b).into_any_element());
            div()
                .when(ix > 0, |d| d.border_t_1().border_color(palette.panel_border))
                .child(person.render(palette, buttons))
        });
        dialog.title("Review members").w(px(460.)).child(
            div()
                .debug_selector(|| "review-members".into())
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_sm().text_color(palette.text_secondary).child(description.clone()))
                .children(rows),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, title: &str, accepted: bool) -> Conversation {
        let mut chat = ReviewPerson {
            aci: id.into(),
            name: title.into(),
            phone: None,
            shared_groups: Vec::new(),
            chat: None,
        }
        .chat_or_new();
        chat.accepted = accepted;
        chat
    }

    #[test]
    fn requests_read_like_desktop() {
        let mut stranger = chat("s", "Riley Park", false);
        assert_eq!(
            request_text(&stranger),
            "Let Riley message you and share your name and photo with them? They won’t know you’ve seen their messages until you accept."
        );
        stranger.blocked = true;
        assert!(request_text(&stranger).ends_with("You won't receive any messages until you unblock them."));
        stranger.kind = ConversationKind::Group;
        assert!(request_text(&stranger).starts_with("Unblock this group"));
        assert_eq!(shared_groups_text(&[]), "No groups in common");
        let groups: Vec<String> = ["A", "B", "C", "D", "E"].map(String::from).to_vec();
        assert_eq!(shared_groups_text(&groups[..1]), "Member of A");
        assert_eq!(shared_groups_text(&groups[..2]), "Member of A and B");
        assert_eq!(shared_groups_text(&groups[..3]), "Member of A, B, and C");
        assert_eq!(shared_groups_text(&groups[..4]), "Member of A, B, C and one more");
        assert_eq!(shared_groups_text(&groups), "Member of A, B, C and 2 more");
    }

    #[test]
    fn a_request_with_a_contacts_name_is_found() {
        let contact = chat("a", "Maya Chen", true);
        let other = chat("b", "Jordan Reyes", true);
        let stranger = chat("c", "Maya Chen", false);
        let all = [contact.clone(), other, stranger.clone()];
        assert_eq!(same_name_contact(&stranger, &all).map(|c| c.id.clone()), Some(contact.id.clone()));
        // Not once it's accepted or blocked, and never for the contact.
        assert!(same_name_contact(&contact, &all).is_none());
        let mut blocked = stranger.clone();
        blocked.blocked = true;
        assert!(same_name_contact(&blocked, &all).is_none());
    }

    #[test]
    fn group_collisions_follow_desktop() {
        let members: Vec<(String, String)> =
            [("me", "Me"), ("a", "Sam"), ("b", "Sam"), ("c", "Ana"), ("d", "Ana"), ("e", "Leo"), ("f", "")]
                .map(|(a, n)| (a.to_owned(), n.to_owned()))
                .to_vec();
        let mut system = HashSet::new();
        system.insert("c".to_owned());
        system.insert("d".to_owned());
        let found = name_collisions(&members, "me", &system);
        // Both Anas are in our address book: no collision.
        assert_eq!(found.keys().collect::<Vec<_>>(), ["Sam"]);
        assert_eq!(group_warning_text(&found), "2 group members have the same name.");
        system.remove("d");
        let found = name_collisions(&members, "me", &system);
        assert_eq!(group_warning_text(&found), "2 name conflicts were found in this group.");
        // Closing the warning acknowledges who it named; someone new shows it again.
        let mut acknowledged = found.clone();
        assert!(!has_unacknowledged(&acknowledged, &found));
        acknowledged.get_mut("Sam").unwrap().pop();
        assert!(has_unacknowledged(&acknowledged, &found));
        assert!(has_unacknowledged(&BTreeMap::new(), &found));
    }

    #[test]
    fn first_names_are_the_titles_first_word() {
        assert_eq!(first_name("Maya Chen"), "Maya");
        assert_eq!(first_name(""), "");
    }
}
