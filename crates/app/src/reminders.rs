//! Desktop's unread reminders (`ts/services/unreadReminders.preload.ts`,
//! `ts/util/unreadReminders.std.ts`): now and then, a notification about
//! unread messages left in a muted chat. Hourly, the chat whose unread
//! messages have waited longest (at least three days since they came or
//! since its last reminder, the newest within two weeks) gets one, at most
//! one chat at a time and each at most every three days.

use std::collections::HashMap;

use signal_store::{ConversationId, UnreadRange};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// `UNREAD_REMINDER_MIN_UNREAD_AGE`.
pub const MIN_UNREAD_AGE_MS: i64 = 3 * DAY_MS;
/// `UNREAD_REMINDER_MIN_INTERVAL_BETWEEN_REMINDERS`.
pub const MIN_INTERVAL_MS: i64 = 3 * DAY_MS;
/// `UNREAD_REMINDER_MAX_AGE`.
pub const MAX_AGE_MS: i64 = 14 * DAY_MS;
/// `UNREAD_REMINDER_POLL_INTERVAL`.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Settings key: when each chat was last reminded (JSON, chat to unix
/// millis; Desktop's `lastUnreadReminderAt`).
pub const REMINDED_KEY: &str = "unread_reminders.reminded";
/// Settings key: when reminders started here (Desktop's
/// `unreadRemindersEnabledAt`): older messages are never reminded of.
pub const ENABLED_AT_KEY: &str = "unread_reminders.enabled_at";

/// When each chat was last reminded.
pub type Reminded = HashMap<String, i64>;

/// Desktop's `filterForConversationsDueReminder`: chats whose unread
/// messages are recent enough and have waited long enough, longest first.
pub fn due(mut ranges: Vec<UnreadRange>, now: i64) -> Vec<ConversationId> {
    ranges.retain(|r| r.newest_unread > now - MAX_AGE_MS && r.oldest_unreminded < now - MIN_UNREAD_AGE_MS);
    ranges.sort_by_key(|r| r.oldest_unreminded);
    ranges.into_iter().map(|r| r.conversation).collect()
}

/// What a reminder says (Desktop's `UnreadReminderSummary`): people by
/// short name, most recent first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub title: String,
    pub unread: u32,
    /// Zero when mentions don't notify while muted.
    pub mentions: u32,
    /// Zero when replies don't notify while muted.
    pub replies: u32,
    pub senders: Vec<String>,
    pub mentioners: Vec<String>,
    pub repliers: Vec<String>,
}

fn plural(n: u32, one: &str, other: &str) -> String {
    if n == 1 { one.replace('#', "1") } else { other.replace('#', &n.to_string()) }
}

fn messages(n: u32) -> String {
    plural(n, "# unread message", "# unread messages")
}

/// Desktop's `getMentionSummary` / `getReplySummary`.
fn people_summary(count: u32, people: &[String], one: [&str; 2], other: &str, by: &str) -> Option<String> {
    let first = people.first()?;
    let what = |single: &str| plural(count, single, other);
    Some(match people {
        [_] => format!("{} {by} {first}", what(one[0])),
        [_, second] => format!("{} {by} {first} and {second}", what(one[1])),
        _ => format!("{} {by} {first} and others", what(one[1])),
    })
}

/// Desktop's `getUnreadReminderNotificationContent`: with names (`full`)
/// the chat's title and who wrote, mentioned or replied; else "Signal" and
/// the count.
pub fn content(summary: &Summary, full: bool) -> (String, String) {
    if !full {
        return ("Signal".into(), format!("You have {}.", messages(summary.unread)));
    }
    let title = if summary.title.is_empty() { "Signal".into() } else { summary.title.clone() };
    let count = messages(summary.unread);
    let mentions = (summary.mentions > 0)
        .then(|| people_summary(summary.mentions, &summary.mentioners, ["a mention", "# mention"], "# mentions", "of you by"))
        .flatten();
    let replies = (summary.replies > 0)
        .then(|| people_summary(summary.replies, &summary.repliers, ["a reply", "# reply"], "# replies", "from"))
        .flatten();
    let body = if let Some(mentions) = mentions {
        format!("You have {count}, including {mentions}.")
    } else if let Some(replies) = replies {
        format!("You have {count}, including {replies}.")
    } else {
        match summary.senders.as_slice() {
            [] => format!("You have {count}."),
            [one] => format!("You have {count} from {one}."),
            [one, two] => format!("You have {count} from {one} and {two}."),
            [one, two, ..] => format!("You have {count} from {one}, {two}, and others."),
        }
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_are_due_as_desktop_picks_them() {
        let now = 100 * DAY_MS;
        let range = |id: &str, oldest_days: i64, newest_days: i64| UnreadRange {
            conversation: ConversationId(id.into()),
            oldest_unreminded: now - oldest_days * DAY_MS,
            newest_unread: now - newest_days * DAY_MS,
        };
        let due = due(vec![range("recent", 1, 0), range("old", 20, 15), range("b", 4, 1), range("a", 10, 2)], now);
        // Too recent and too old are left out; longest waiting first.
        assert_eq!(due, vec![ConversationId("a".into()), ConversationId("b".into())]);
    }

    #[test]
    fn content_reads_as_desktops() {
        let mut s = Summary { title: "Hikers".into(), unread: 1, senders: vec!["Maya".into()], ..Default::default() };
        assert_eq!(content(&s, true), ("Hikers".into(), "You have 1 unread message from Maya.".into()));
        assert_eq!(content(&s, false), ("Signal".into(), "You have 1 unread message.".into()));
        s.unread = 5;
        s.senders = vec!["Maya".into(), "Jordan".into()];
        assert_eq!(content(&s, true).1, "You have 5 unread messages from Maya and Jordan.");
        s.senders.push("Priya".into());
        assert_eq!(content(&s, true).1, "You have 5 unread messages from Maya, Jordan, and others.");
        s.replies = 1;
        s.repliers = vec!["Sam".into()];
        assert_eq!(content(&s, true).1, "You have 5 unread messages, including a reply from Sam.");
        // Mentions come before replies.
        s.mentions = 2;
        s.mentioners = vec!["Ana".into(), "Leo".into()];
        assert_eq!(content(&s, true).1, "You have 5 unread messages, including 2 mentions of you by Ana and Leo.");
        s.mentioners.push("Kim".into());
        assert_eq!(content(&s, true).1, "You have 5 unread messages, including 2 mentions of you by Ana and others.");
        s.mentions = 1;
        s.mentioners = vec!["Ana".into()];
        assert_eq!(content(&s, true).1, "You have 5 unread messages, including a mention of you by Ana.");
    }
}
