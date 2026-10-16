//! Runs of updates folded into one row, as Signal Desktop's timeline does
//! (`mapItemsIntoCollapseSets` in `ts/util/CollapseSet.std.ts`, called from
//! `ts/state/smart/Timeline.preload.tsx` with `allowMultidaySets: false`,
//! and drawn by `CollapseSetViewer` in
//! `ts/components/conversation/CollapseSet.dom.tsx`).
//!
//! Consecutive updates of a day fold into "N group updates" (group changes
//! but the group's end, profile, safety number, verification and number
//! changes, pins and poll ends), "N disappearing message timer changes" or
//! "N call events". A set breaks at the unread marker and at a new day, and
//! holds at most [`MAX_COLLAPSE_SET_SIZE`] items.
//!
//! Here each detail of a group change is a notice of its own, where Desktop
//! keeps one message with several details: a change with several details
//! is still a set by itself (its notices are consecutive updates), but
//! each detail counts toward the set's size, and a change that ended the
//! group keeps its other details collapsible (Desktop checks only its first
//! detail).

use signal_store::{GroupNotice, Message, Notice};

/// Desktop's `MAX_COLLAPSE_SET_SIZE`.
pub const MAX_COLLAPSE_SET_SIZE: usize = 50;

/// Which kind of set an item can join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Not collapsible (Desktop's `none`).
    None,
    /// Desktop's `group-updates` (`canCollapseForGroupSet`).
    Updates,
    /// Desktop's `timer-changes`, with the timer it set (seconds; 0: off).
    Timer(u32),
    /// Desktop's `call-events`.
    Calls,
}

/// What an item in the timeline is, for collapsing. `in_call`: a call in
/// this chat is going on now, whose notice Desktop leaves out of sets
/// (`canCollapseForCallSet`; here the chat's newest call notice stands for
/// it, since its call id isn't compared to the live call's).
pub fn kind_of(message: &Message, in_call: bool) -> Kind {
    let Some(notice) = &message.notice else { return Kind::None };
    match notice {
        Notice::Group { change: GroupNotice::Ended, .. } => Kind::None,
        Notice::Group { .. }
        | Notice::ProfileNameChanged { .. }
        | Notice::SafetyNumberChanged { .. }
        | Notice::Verified { .. }
        | Notice::NumberChanged { .. }
        | Notice::Pinned { .. }
        | Notice::PollEnded { .. } => Kind::Updates,
        Notice::Timer { seconds, .. } => Kind::Timer(*seconds),
        Notice::Call { .. } | Notice::GroupCall { .. } if in_call => Kind::None,
        Notice::Call { .. } | Notice::GroupCall { .. } => Kind::Calls,
        _ => Kind::None,
    }
}

fn same_kind(a: Kind, b: Kind) -> bool {
    matches!(
        (a, b),
        (Kind::Updates, Kind::Updates) | (Kind::Timer(_), Kind::Timer(_)) | (Kind::Calls, Kind::Calls)
    )
}

/// A set of consecutive items (indices into the timeline), as Desktop's
/// `CollapseSet`; a `Kind::None` set is one item shown as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapseSet {
    pub kind: Kind,
    pub start: usize,
    pub len: usize,
}

impl CollapseSet {
    /// The timer the set ends on (Desktop's `endingState`), for a timer set.
    pub fn ending_timer(&self, kinds: &[Kind]) -> Option<u32> {
        match kinds.get(self.start + self.len - 1) {
            Some(Kind::Timer(seconds)) => Some(*seconds),
            _ => None,
        }
    }
}

/// Desktop's `mapItemsIntoCollapseSets` without multi-day sets: `kinds`
/// and `days` (a day number per item) in timeline order, and the index of
/// the oldest unseen item (the unread marker), where a set starts afresh.
pub fn collapse_sets(kinds: &[Kind], days: &[i64], oldest_unseen: Option<usize>) -> Vec<CollapseSet> {
    let mut sets: Vec<CollapseSet> = Vec::new();
    for i in 0..kinds.len() {
        let single = CollapseSet { kind: Kind::None, start: i, len: 1 };
        if i == 0 || Some(i) == oldest_unseen || days[i] != days[i - 1] {
            sets.push(single);
            continue;
        }
        let last = sets.last_mut().expect("a set before the second item");
        let room = last.kind == Kind::None || last.len < MAX_COLLAPSE_SET_SIZE;
        if room && kinds[i] != Kind::None && same_kind(kinds[i], kinds[i - 1]) {
            if last.kind == Kind::None {
                last.kind = kinds[i];
            }
            last.len += 1;
            continue;
        }
        sets.push(single);
    }
    // A set's kind is its last item's (the timer it ends on).
    for set in &mut sets {
        if set.kind != Kind::None {
            set.kind = kinds[set.start + set.len - 1];
        }
    }
    sets
}

/// How `CollapseSetViewer` shows a set: the items folded behind its button
/// (`collapsed`, from its start; empty when there's no button), and the
/// rest shown as they are. `unseen` says whether an item was unseen when it
/// was first shown. The button needs two folded items: Desktop shows it
/// unless the set's first unseen item is its first or second.
pub fn folded(set: &CollapseSet, unseen: impl Fn(usize) -> bool) -> usize {
    if set.kind == Kind::None {
        return 0;
    }
    let first_unseen = (0..set.len).find(|&i| unseen(set.start + i));
    match first_unseen {
        None => set.len,
        Some(i) if i > 1 => i,
        Some(_) => 0,
    }
}

/// The button's words, after Desktop's `CollapseSetButton`
/// (`icu:collapsedGroupUpdates`, `icu:collapsedChatUpdates`,
/// `icu:collapsedTimerChanges`, `icu:collapsedCallEvents`); `group`: the
/// chat is a group, `timer` the timer the set ends on.
pub fn button_text(kind: Kind, count: usize, group: bool, timer: Option<u32>) -> String {
    let plural = |one: &str, other: &str| if count == 1 { format!("{count} {one}") } else { format!("{count} {other}") };
    match kind {
        Kind::Updates if group => plural("group update", "group updates"),
        Kind::Updates => plural("chat update", "chat updates"),
        Kind::Timer(_) => {
            let changes = plural("disappearing timer change", "disappearing message timer changes");
            match timer.filter(|t| *t > 0) {
                Some(seconds) => format!("{changes} \u{b7} {}", crate::views::chat_menu::format_timer_long(seconds)),
                None => format!("{changes} \u{b7} Disabled"),
            }
        }
        Kind::Calls => plural("call event", "call events"),
        Kind::None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: Kind = Kind::Updates;
    const N: Kind = Kind::None;
    const C: Kind = Kind::Calls;

    fn sets(kinds: &[Kind], days: &[i64], unseen: Option<usize>) -> Vec<(Kind, usize, usize)> {
        collapse_sets(kinds, days, unseen).into_iter().map(|s| (s.kind, s.start, s.len)).collect()
    }

    #[test]
    fn consecutive_updates_of_a_kind_fold() {
        let kinds = [N, U, U, U, N, Kind::Timer(60), Kind::Timer(0), C, C, U];
        assert_eq!(
            sets(&kinds, &[0; 10], None),
            [(N, 0, 1), (U, 1, 3), (N, 4, 1), (Kind::Timer(0), 5, 2), (C, 7, 2), (N, 9, 1)]
        );
        // A lone update is a set by itself (shown as it is).
        assert_eq!(sets(&[U], &[0], None), [(N, 0, 1)]);
    }

    #[test]
    fn sets_break_at_a_new_day_and_the_unread_marker() {
        let kinds = [U, U, U, U, U];
        assert_eq!(sets(&kinds, &[0, 0, 1, 1, 1], None), [(U, 0, 2), (U, 2, 3)]);
        assert_eq!(sets(&kinds, &[0; 5], Some(3)), [(U, 0, 3), (U, 3, 2)]);
    }

    #[test]
    fn sets_hold_at_most_fifty() {
        let kinds = vec![U; 120];
        let days = vec![0; 120];
        assert_eq!(sets(&kinds, &days, None), [(U, 0, 50), (U, 50, 50), (U, 100, 20)]);
    }

    #[test]
    fn unseen_items_stay_out_of_the_fold() {
        let set = CollapseSet { kind: U, start: 10, len: 5 };
        assert_eq!(folded(&set, |_| false), 5);
        assert_eq!(folded(&set, |i| i >= 13), 3);
        // Only one seen: no button, all shown.
        assert_eq!(folded(&set, |i| i >= 11), 0);
        assert_eq!(folded(&CollapseSet { kind: N, start: 0, len: 1 }, |_| false), 0);
    }

    #[test]
    fn buttons_read_like_desktop() {
        assert_eq!(button_text(U, 3, true, None), "3 group updates");
        assert_eq!(button_text(U, 2, false, None), "2 chat updates");
        assert_eq!(button_text(Kind::Timer(0), 2, true, Some(0)), "2 disappearing message timer changes \u{b7} Disabled");
        assert_eq!(button_text(Kind::Timer(86_400), 4, false, Some(86_400)), "4 disappearing message timer changes \u{b7} 1 day");
        assert_eq!(button_text(C, 2, false, None), "2 call events");
    }
}
