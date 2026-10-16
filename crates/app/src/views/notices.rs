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
    }
}
