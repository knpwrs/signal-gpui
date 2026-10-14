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
        GroupNotice::Ended => format!("{} ended the group.", actor()),
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
        let group = |by: Option<String>, change| Notice::Group { by, change };
        assert_eq!(
            text(group(by("a"), GroupNotice::Title { title: "Hikers".into() })),
            "Alice changed the group name to \"Hikers\"."
        );
        assert_eq!(text(group(by("b"), GroupNotice::Removed { member: by("b") })), "Bob left the group.");
        assert_eq!(text(group(None, GroupNotice::Removed { member: None })), "You left the group.");
        assert_eq!(text(group(by("a"), GroupNotice::Removed { member: None })), "Alice removed you.");
        assert_eq!(text(group(None, GroupNotice::Added { member: by("b") })), "You added Bob.");
        assert_eq!(text(group(by("a"), GroupNotice::Admin { member: by("b"), admin: true })), "Alice made Bob an admin.");
    }
}
