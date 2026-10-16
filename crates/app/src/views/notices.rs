//! What timeline notices say, in Signal Desktop's words.

use signal_store::{GroupNotice, Notice};

use crate::views::chat_menu::format_timer_long;

/// `name(sid)` gives a person's display name (`sid` is never `None`).
pub fn notice_text(notice: &Notice, name: impl Fn(&str) -> String) -> String {
    match notice {
        Notice::Timer { by: None, seconds: 0 } => "You disabled disappearing messages.".into(),
        Notice::Timer { by: Some(by), seconds: 0 } => format!("{} disabled disappearing messages.", name(by)),
        Notice::Timer { by: None, seconds } => {
            format!("You set the disappearing message time to {}.", format_timer_long(*seconds))
        }
        Notice::Timer { by: Some(by), seconds } => {
            format!("{} set the disappearing message time to {}.", name(by), format_timer_long(*seconds))
        }
        Notice::Pinned { by: None, .. } => "You pinned a message".into(),
        Notice::Pinned { by: Some(by), .. } => format!("{} pinned a message", name(by)),
        Notice::Group { by, change } => group_text(by.as_deref(), change, name),
        Notice::SafetyNumberChanged { who } => format!("Your safety number with {} has changed.", name(who)),
        Notice::Call { incoming, video, outcome, .. } => call_text(*incoming, *video, *outcome),
        notice @ Notice::GroupCall { by, status, .. } => group_call_text(notice.group_call_incoming(), by.as_deref(), *status, name),
        // Desktop's `icu:PollTerminate--you` / `--other`.
        Notice::PollEnded { by: None, question, .. } => format!("You ended the poll: {question}"),
        Notice::PollEnded { by: Some(by), question, .. } => format!("{} ended the poll: {question}", name(by)),
        Notice::MessageRequest { event, group, who } => message_request_text(*event, *group, who.as_deref(), name),
        // Without their address book name; see `profile_change_text`.
        Notice::ProfileNameChanged { old_name, new_name, .. } => profile_change_text(old_name, new_name, None),
        // Desktop's `icu:ChangeNumber--notification`.
        Notice::NumberChanged { who } => format!("{} changed their phone number", name(who)),
        // `icu:ChatRefresh--notification`.
        Notice::SessionRefreshed => "Chat session refreshed".into(),
        // `icu:DeliveryIssue--notification`.
        Notice::DeliveryIssue { who } => format!("A message from {} couldn\u{2019}t be delivered", name(who)),
        // `icu:Message--unsupported-message` and `--from-me-unsupported-message`.
        Notice::UnsupportedMessage { who: Some(who) } => format!(
            "{} sent you a message that can't be processed or displayed because it uses a new Signal feature.",
            name(who)
        ),
        Notice::UnsupportedMessage { who: None } => {
            "One of your devices sent a message that can't be processed or displayed because it uses a new Signal feature.".into()
        }
        // `icu:youMarkedAsVerified` and the rest (`VerificationNotification`).
        Notice::Verified { who, verified, local } => format!(
            "You marked your Safety Number with {} as {}{}",
            name(who),
            if *verified { "verified" } else { "not verified" },
            if *local { "" } else { " from another device" }
        ),
    }
}

/// Desktop's `getStringForProfileChange`: `icu:contactChangedProfileName`
/// with the name we know them by (`sender`, when we saved a name for them),
/// else `icu:changedProfileName`.
pub fn profile_change_text(old_name: &str, new_name: &str, sender: Option<&str>) -> String {
    match sender {
        Some(sender) => format!("{sender} changed their profile name from {old_name} to {new_name}."),
        None => format!("{old_name} changed their profile name to {new_name}."),
    }
}

/// The icon Desktop's `SystemMessage` shows beside a notice, for the
/// notices that have one here.
pub fn notice_icon(notice: &Notice) -> Option<gpui_kit::assets::IconName> {
    use gpui_kit::assets::IconName;
    Some(match notice {
        // Desktop's `profile` icon.
        Notice::ProfileNameChanged { .. } => IconName::User,
        Notice::NumberChanged { .. } => IconName::Phone,
        // `arrow-clockwise`.
        Notice::SessionRefreshed => IconName::RotateCcw,
        // `info-circle`.
        Notice::DeliveryIssue { .. } => IconName::Info,
        Notice::UnsupportedMessage { .. } => IconName::CircleAlert,
        // `verified` and `verified-not`.
        Notice::Verified { verified: true, .. } => IconName::BadgeCheck,
        Notice::Verified { verified: false, .. } => IconName::CircleAlert,
        _ => return None,
    })
}

/// Desktop's `ChatSessionRefreshedDialog` and `DeliveryIssueDialog`
/// support pages.
const CHAT_REFRESHED_SUPPORT: &str = "https://support.signal.org/hc/requests/new?desktop&chat_refreshed";
const DELIVERY_ISSUE_SUPPORT: &str = "https://support.signal.org/hc/articles/4404859745690";

/// Desktop's "Learn More" dialog for a chat session refreshed notice
/// (`ChatSessionRefreshedDialog`): what happened, "Contact Support" and OK.
pub fn open_session_refreshed(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    learn_more_dialog(
        "Chat session refreshed",
        "Signal uses end-to-end encryption and it may need to refresh your chat session sometimes. This doesn\u{2019}t affect your chat\u{2019}s security but you may have missed a message from this contact and you can ask them to resend it.".into(),
        ("Contact Support", CHAT_REFRESHED_SUPPORT),
        window,
        cx,
    );
}

/// Desktop's `DeliveryIssueDialog`: `sender` by name, in a group or not.
pub fn open_delivery_issue(sender: &str, group: bool, window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    let summary = if group {
        format!("A message, sticker, reaction, read receipt or media couldn\u{2019}t be delivered to you from {sender} in this chat.")
    } else {
        format!(
            "A message, sticker, reaction, read receipt or media couldn\u{2019}t be delivered to you from {sender}. They may have tried sending it to you directly, or in a group."
        )
    };
    learn_more_dialog("Delivery Issue", summary, ("Learn More", DELIVERY_ISSUE_SUPPORT), window, cx);
}

fn learn_more_dialog(
    title: &'static str,
    summary: String,
    (link_label, url): (&'static str, &'static str),
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    use gpui_kit::prelude::*;
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = crate::theme::Palette::get(cx);
        dialog
            .title(title)
            .w(gpui_kit::px(400.))
            .child(
                gpui_kit::div()
                    .debug_selector(move || format!("learn-more-{title}"))
                    .text_sm()
                    .text_center()
                    .text_color(palette.text_secondary)
                    .child(summary.clone()),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("learn-more-link").label(link_label).outline().on_click(move |_, _, cx| cx.open_url(url)))
                    .child(Button::new("learn-more-ok").label("OK").primary().on_click(|_, window, cx| window.close_all_dialogs(cx))),
            )
    });
}

/// Desktop's `MessageRequestResponseNotification` wording.
fn message_request_text(event: signal_store::MessageRequestEvent, group: bool, who: Option<&str>, name: impl Fn(&str) -> String) -> String {
    use signal_store::MessageRequestEvent;
    match (event, group, who) {
        (MessageRequestEvent::Accept, false, Some(who)) => format!("You accepted {}'s message request", name(who)),
        (MessageRequestEvent::Accept, _, _) => "You accepted the message request".into(),
        (MessageRequestEvent::Block, true, _) => "You blocked the group".into(),
        (MessageRequestEvent::Block, false, _) => "You blocked this person".into(),
        (MessageRequestEvent::Unblock, true, _) => "You unblocked the group".into(),
        (MessageRequestEvent::Unblock, false, _) => "You unblocked this person".into(),
        (MessageRequestEvent::Spam, _, _) => "Reported as spam".into(),
    }
}

/// A group call's notice, after Desktop's `getGroupCallNotificationText`:
/// one rung and missed or declined says so; otherwise who started it.
/// (Desktop also says "The video call has ended" once it's over, which
/// needs the call's live state; this doesn't have it.)
fn group_call_text(
    incoming: bool,
    by: Option<&str>,
    status: Option<signal_store::GroupCallStatus>,
    name: impl Fn(&str) -> String,
) -> String {
    use signal_store::GroupCallStatus;
    match (incoming, status, by) {
        (true, Some(GroupCallStatus::Missed), _) => "Missed group call".into(),
        (true, Some(GroupCallStatus::Declined), _) => "Declined group call".into(),
        (false, _, _) => "You started a group call".into(),
        (true, _, Some(by)) => format!("{} started a group call", name(by)),
        (true, _, None) => "A group call was started".into(),
    }
}

/// Desktop's call history wording (`icu:missedIncomingAudioCall` and the rest).
pub fn call_text(incoming: bool, video: bool, outcome: signal_store::CallOutcome) -> String {
    use signal_store::CallOutcome;
    let kind = if video { "video" } else { "voice" };
    match outcome {
        CallOutcome::Accepted if incoming => format!("Incoming {kind} call"),
        CallOutcome::Accepted => format!("Outgoing {kind} call"),
        CallOutcome::Missed => format!("Missed {kind} call"),
        CallOutcome::Declined => format!("Declined {kind} call"),
        CallOutcome::Unanswered => format!("Unanswered {kind} call"),
    }
}

fn group_text(by: Option<&str>, change: &GroupNotice, name: impl Fn(&str) -> String) -> String {
    let who = by.map(&name);
    // "You …" or "Alice …".
    let actor = || who.clone().unwrap_or_else(|| "You".into());
    let admins_only = |only: bool| if only { "Only admins." } else { "All members." };
    match change {
        GroupNotice::Title { title } if title.is_empty() => format!("{} removed the group name.", actor()),
        GroupNotice::Title { title } => format!("{} changed the group name to \"{title}\".", actor()),
        GroupNotice::Description => format!("{} changed the group description.", actor()),
        GroupNotice::Avatar => format!("{} changed the group avatar.", actor()),
        GroupNotice::Added { member: None } if by.is_none() => "You joined the group.".into(),
        GroupNotice::Added { member: None } => format!("{} added you to the group.", actor()),
        GroupNotice::Added { member: Some(m) } if by.is_none() => format!("You added {}.", name(m)),
        GroupNotice::Added { member: Some(m) } if by == Some(m.as_str()) => format!("{} joined the group.", name(m)),
        GroupNotice::Added { member: Some(m) } => format!("{} added {}.", actor(), name(m)),
        GroupNotice::Removed { member: None } if by.is_none() => "You left the group.".into(),
        GroupNotice::Removed { member: None } => format!("{} removed you.", actor()),
        GroupNotice::Removed { member: Some(m) } if by == Some(m.as_str()) => format!("{} left the group.", name(m)),
        GroupNotice::Removed { member: Some(m) } if by.is_none() => format!("You removed {}.", name(m)),
        GroupNotice::Removed { member: Some(m) } => format!("{} removed {}.", actor(), name(m)),
        GroupNotice::Admin { member: None, admin: true } => format!("{} made you an admin.", actor()),
        GroupNotice::Admin { member: None, admin: false } => format!("{} revoked your admin privileges.", actor()),
        GroupNotice::Admin { member: Some(m), admin: true } => format!("{} made {} an admin.", actor(), name(m)),
        GroupNotice::Admin { member: Some(m), admin: false } => {
            format!("{} revoked admin privileges from {}.", actor(), name(m))
        }
        GroupNotice::InfoAccess { admins_only: only } => {
            format!("{} changed who can edit group info to \"{}\"", actor(), admins_only(*only))
        }
        GroupNotice::MemberAccess { admins_only: only } => {
            format!("{} changed who can edit group membership to \"{}\"", actor(), admins_only(*only))
        }
        GroupNotice::AnnouncementsOnly { on: true } => {
            format!("{} changed the group settings to only allow admins to send messages.", actor())
        }
        GroupNotice::AnnouncementsOnly { on: false } => {
            format!("{} changed the group settings to allow all members to send messages.", actor())
        }
        // Desktop's `GroupV2--terminated--you` and `--other`, without a period.
        GroupNotice::Ended => format!("{} ended the group", actor()),
        _ => membership_text(by, change, name),
    }
}

/// "1 person" or "3 people".
fn people(count: u32) -> String {
    if count == 1 { "1 person".into() } else { format!("{count} people") }
}

/// "an invitation to the group for 1 person" or "invitations to the
/// group for 3 people".
fn invitations(count: u32) -> String {
    if count == 1 { "an invitation to the group for 1 person".into() } else { format!("invitations to the group for {count} people") }
}

/// Invitations, requests to join and the group link, in Desktop's words
/// (`renderChangeDetail` in `ts/groupChange.std.ts`, `icu:GroupV2--…`).
fn membership_text(by: Option<&str>, change: &GroupNotice, name: impl Fn(&str) -> String) -> String {
    use signal_store::Inviter;
    let who = by.map(&name);
    let actor = || who.clone().unwrap_or_else(|| "You".into());
    let same = |member: &Option<String>| member.as_deref() == by;
    match change {
        GroupNotice::JoinedByLink { member: None } => "You joined the group via the group link.".into(),
        GroupNotice::JoinedByLink { member: Some(m) } => format!("{} joined the group via the group link.", name(m)),
        GroupNotice::AcceptedInvite { member: None, inviter: Inviter::Person { id } } => {
            format!("You accepted an invitation to the group from {}.", name(id))
        }
        GroupNotice::AcceptedInvite { member: None, .. } => "You accepted an invitation to the group.".into(),
        GroupNotice::AcceptedInvite { member: Some(m), inviter: Inviter::Us } => {
            format!("{} accepted your invitation to the group.", name(m))
        }
        GroupNotice::AcceptedInvite { member: Some(m), inviter: Inviter::Person { id } } => {
            format!("{} accepted an invitation to the group from {}.", name(m), name(id))
        }
        GroupNotice::AcceptedInvite { member: Some(m), inviter: Inviter::Unknown } => {
            format!("{} accepted an invitation to the group.", name(m))
        }
        GroupNotice::Approved { member: None } => match &who {
            Some(admin) => format!("{admin} approved your request to join the group."),
            None => "Your request to join the group has been approved.".into(),
        },
        GroupNotice::Approved { member: Some(m) } => match &who {
            Some(admin) => format!("{admin} approved a request to join the group from {}.", name(m)),
            None => format!("You approved a request to join the group from {}.", name(m)),
        },
        GroupNotice::Invited { member: None } => match &who {
            Some(admin) => format!("{admin} invited you to the group."),
            None => "You were invited to the group.".into(),
        },
        GroupNotice::Invited { member: Some(m) } => match &who {
            Some(member) => format!("{member} invited 1 person to the group."),
            None => format!("You invited {} to the group.", name(m)),
        },
        GroupNotice::InvitedMany { count } => format!("{} invited {} to the group.", actor(), people(*count)),
        GroupNotice::InviteRemoved { member, inviter } => {
            let invitee = || member.as_deref().map_or_else(|| "you".to_owned(), &name);
            match (inviter, same(member)) {
                (Inviter::Us, true) => format!("{} declined your invitation to the group.", invitee()),
                (Inviter::Us, false) => match &who {
                    Some(admin) => format!("{admin} revoked the invitation to the group you sent to {}.", invitee()),
                    None => format!("You rescinded your invitation to {}.", invitee()),
                },
                (_, true) if by.is_none() => "You declined the invitation to the group.".into(),
                (Inviter::Person { id }, true) => format!("1 person invited by {} declined the invitation to the group.", name(id)),
                (_, true) => "1 person declined their invitation to the group.".into(),
                (Inviter::Person { id }, false) if by == Some(id.as_str()) => match member {
                    None => format!("{} revoked their invitation to you.", name(id)),
                    Some(_) => format!("{} revoked their invitation to 1 person.", name(id)),
                },
                (Inviter::Person { id }, false) => {
                    format!("{} revoked an invitation to the group for 1 person invited by {}.", actor(), name(id))
                }
                (Inviter::Unknown, false) => format!("{} revoked an invitation to the group for 1 person.", actor()),
            }
        }
        GroupNotice::InvitesRemoved { count, inviter } => match inviter {
            Inviter::Us => match &who {
                Some(admin) => format!(
                    "{admin} revoked the {}.",
                    if *count == 1 {
                        "invitation to the group you sent to 1 person".to_owned()
                    } else {
                        format!("invitations to the group you sent to {count} people")
                    }
                ),
                None => format!("You rescinded your invitation to {}.", people(*count)),
            },
            Inviter::Person { id } => format!("{} revoked {} invited by {}.", actor(), invitations(*count), name(id)),
            Inviter::Unknown => format!("{} revoked {}.", actor(), invitations(*count)),
        },
        GroupNotice::Requested { member: None } => "You sent a request to join the group.".into(),
        GroupNotice::Requested { member: Some(m) } => format!("{} requested to join via the group link.", name(m)),
        GroupNotice::RequestRemoved { member: None } if by.is_none() => "You canceled your request to join the group.".into(),
        GroupNotice::RequestRemoved { member: None } => "Your request to join the group has been denied by an admin.".into(),
        GroupNotice::RequestRemoved { member: Some(m) } if by == Some(m.as_str()) => {
            format!("{} canceled their request to join the group.", name(m))
        }
        GroupNotice::RequestRemoved { member: Some(m) } => match &who {
            Some(admin) => format!("{admin} denied a request to join the group from {}.", name(m)),
            None => format!("You denied a request to join the group from {}.", name(m)),
        },
        GroupNotice::LinkOn { approval } => format!(
            "{} turned on the group link with admin approval {}.",
            actor(),
            if *approval { "enabled" } else { "disabled" }
        ),
        GroupNotice::LinkOff => format!("{} turned off the group link.", actor()),
        GroupNotice::LinkReset => format!("{} reset the group link.", actor()),
        GroupNotice::LinkApproval { on } => {
            format!("{} {} admin approval for the group link.", actor(), if *on { "enabled" } else { "disabled" })
        }
        // The rest are `group_text`'s.
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(notice: Notice) -> String {
        notice_text(&notice, |sid| if sid == "a" { "Alice".into() } else { "Bob".into() })
    }

    #[test]
    fn notices_read_like_desktop() {
        let by = |s: &str| Some(s.to_owned());
        assert_eq!(text(Notice::Timer { by: None, seconds: 86_400 }), "You set the disappearing message time to 1 day.");
        assert_eq!(text(Notice::Timer { by: by("a"), seconds: 0 }), "Alice disabled disappearing messages.");
        use signal_store::CallOutcome;
        let call = |incoming, video, outcome| text(Notice::Call { incoming, video, outcome, call_id: None, unseen: false });
        assert_eq!(call(true, false, CallOutcome::Missed), "Missed voice call");
        assert_eq!(call(false, true, CallOutcome::Accepted), "Outgoing video call");
        assert_eq!(call(true, true, CallOutcome::Accepted), "Incoming video call");
        assert_eq!(call(true, false, CallOutcome::Declined), "Declined voice call");
        assert_eq!(call(false, false, CallOutcome::Unanswered), "Unanswered voice call");
        use signal_store::GroupCallStatus;
        let group_call = |by: Option<String>, incoming, status| {
            text(Notice::GroupCall { by, era: None, call_id: Some(1), status: Some(status), incoming, unseen: false })
        };
        assert_eq!(group_call(None, false, GroupCallStatus::Ringing), "You started a group call");
        assert_eq!(group_call(by("a"), true, GroupCallStatus::Joined), "Alice started a group call");
        assert_eq!(group_call(None, true, GroupCallStatus::Accepted), "A group call was started");
        assert_eq!(group_call(by("a"), true, GroupCallStatus::Missed), "Missed group call");
        assert_eq!(group_call(by("a"), true, GroupCallStatus::Declined), "Declined group call");
        // Notices from before statuses: who started it.
        let old = |by| text(Notice::GroupCall { by, era: None, call_id: None, status: None, incoming: false, unseen: false });
        assert_eq!(old(by("b")), "Bob started a group call");
        assert_eq!(old(None), "You started a group call");
        assert_eq!(
            text(Notice::SafetyNumberChanged { who: "a".into() }),
            "Your safety number with Alice has changed."
        );
        let group = |by: Option<String>, change| Notice::Group { by, change };
        assert_eq!(
            text(group(by("a"), GroupNotice::Title { title: "Hikers".into() })),
            "Alice changed the group name to \"Hikers\"."
        );
        assert_eq!(text(group(by("b"), GroupNotice::Removed { member: by("b") })), "Bob left the group.");
        assert_eq!(text(group(None, GroupNotice::Ended)), "You ended the group");
        assert_eq!(text(group(by("a"), GroupNotice::Ended)), "Alice ended the group");
        assert_eq!(text(group(None, GroupNotice::Removed { member: None })), "You left the group.");
        assert_eq!(text(group(by("a"), GroupNotice::Removed { member: None })), "Alice removed you.");
        assert_eq!(text(group(None, GroupNotice::Added { member: by("b") })), "You added Bob.");
        assert_eq!(text(group(by("a"), GroupNotice::Admin { member: by("b"), admin: true })), "Alice made Bob an admin.");
        use signal_store::MessageRequestEvent as E;
        let request = |event, group, who: Option<String>| text(Notice::MessageRequest { event, group, who });
        assert_eq!(request(E::Accept, false, by("a")), "You accepted Alice's message request");
        assert_eq!(request(E::Accept, true, None), "You accepted the message request");
        assert_eq!(request(E::Block, false, by("a")), "You blocked this person");
        assert_eq!(request(E::Block, true, None), "You blocked the group");
        assert_eq!(request(E::Unblock, false, by("a")), "You unblocked this person");
        assert_eq!(request(E::Spam, false, by("a")), "Reported as spam");
    }

    #[test]
    fn identity_and_profile_notices_read_like_desktop() {
        let profile = Notice::ProfileNameChanged { who: "a".into(), old_name: "Al".into(), new_name: "Alice B".into() };
        assert_eq!(text(profile), "Al changed their profile name to Alice B.");
        assert_eq!(profile_change_text("Al", "Alice B", Some("Alice")), "Alice changed their profile name from Al to Alice B.");
        assert_eq!(text(Notice::NumberChanged { who: "a".into() }), "Alice changed their phone number");
        assert_eq!(text(Notice::SessionRefreshed), "Chat session refreshed");
        assert_eq!(text(Notice::DeliveryIssue { who: "b".into() }), "A message from Bob couldn\u{2019}t be delivered");
        assert_eq!(
            text(Notice::UnsupportedMessage { who: Some("a".into()) }),
            "Alice sent you a message that can't be processed or displayed because it uses a new Signal feature."
        );
        assert!(text(Notice::UnsupportedMessage { who: None }).starts_with("One of your devices sent a message"));
        let verified = |verified, local| text(Notice::Verified { who: "a".into(), verified, local });
        assert_eq!(verified(true, true), "You marked your Safety Number with Alice as verified");
        assert_eq!(verified(false, true), "You marked your Safety Number with Alice as not verified");
        assert_eq!(verified(true, false), "You marked your Safety Number with Alice as verified from another device");
        assert_eq!(verified(false, false), "You marked your Safety Number with Alice as not verified from another device");
    }

    #[test]
    fn invitations_requests_and_links_read_like_desktop() {
        use signal_store::Inviter;
        let by = |s: &str| Some(s.to_owned());
        let group = |by: Option<String>, change| text(Notice::Group { by, change });
        let alice = || Inviter::Person { id: "a".into() };
        assert_eq!(group(by("a"), GroupNotice::Invited { member: None }), "Alice invited you to the group.");
        assert_eq!(group(None, GroupNotice::Invited { member: by("b") }), "You invited Bob to the group.");
        assert_eq!(group(by("a"), GroupNotice::Invited { member: by("b") }), "Alice invited 1 person to the group.");
        assert_eq!(group(by("a"), GroupNotice::InvitedMany { count: 3 }), "Alice invited 3 people to the group.");
        assert_eq!(
            group(None, GroupNotice::AcceptedInvite { member: None, inviter: alice() }),
            "You accepted an invitation to the group from Alice."
        );
        assert_eq!(
            group(by("b"), GroupNotice::AcceptedInvite { member: by("b"), inviter: Inviter::Us }),
            "Bob accepted your invitation to the group."
        );
        assert_eq!(group(None, GroupNotice::InviteRemoved { member: None, inviter: alice() }), "You declined the invitation to the group.");
        assert_eq!(
            group(by("b"), GroupNotice::InviteRemoved { member: by("b"), inviter: Inviter::Us }),
            "Bob declined your invitation to the group."
        );
        assert_eq!(group(None, GroupNotice::InviteRemoved { member: by("b"), inviter: Inviter::Us }), "You rescinded your invitation to Bob.");
        assert_eq!(group(by("a"), GroupNotice::InviteRemoved { member: None, inviter: alice() }), "Alice revoked their invitation to you.");
        assert_eq!(
            group(None, GroupNotice::InviteRemoved { member: by("b"), inviter: alice() }),
            "You revoked an invitation to the group for 1 person invited by Alice."
        );
        assert_eq!(
            group(by("a"), GroupNotice::InvitesRemoved { count: 2, inviter: Inviter::Unknown }),
            "Alice revoked invitations to the group for 2 people."
        );
        assert_eq!(group(None, GroupNotice::Requested { member: None }), "You sent a request to join the group.");
        assert_eq!(group(by("b"), GroupNotice::Requested { member: by("b") }), "Bob requested to join via the group link.");
        assert_eq!(group(None, GroupNotice::RequestRemoved { member: by("b") }), "You denied a request to join the group from Bob.");
        assert_eq!(group(by("b"), GroupNotice::RequestRemoved { member: by("b") }), "Bob canceled their request to join the group.");
        assert_eq!(group(None, GroupNotice::RequestRemoved { member: None }), "You canceled your request to join the group.");
        assert_eq!(
            group(by("a"), GroupNotice::RequestRemoved { member: None }),
            "Your request to join the group has been denied by an admin."
        );
        assert_eq!(group(by("a"), GroupNotice::Approved { member: None }), "Alice approved your request to join the group.");
        assert_eq!(group(by("b"), GroupNotice::JoinedByLink { member: by("b") }), "Bob joined the group via the group link.");
        assert_eq!(group(None, GroupNotice::LinkOn { approval: true }), "You turned on the group link with admin approval enabled.");
        assert_eq!(group(by("a"), GroupNotice::LinkOff), "Alice turned off the group link.");
        assert_eq!(group(by("a"), GroupNotice::LinkReset), "Alice reset the group link.");
        assert_eq!(group(None, GroupNotice::LinkApproval { on: false }), "You disabled admin approval for the group link.");
    }
}
