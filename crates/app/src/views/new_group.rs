//! Desktop's "New group" (pick members, name it, create) and "Add members"
//! (pick people, confirm), both within the remote config's group size
//! limits: rows stop being pickable at the hard limit, and Desktop's
//! one-time alerts say when the recommended or the hard limit is reached.

use std::collections::BTreeSet;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, SharedString, Window, div, px};
use signal_backend::Command;
use signal_store::{Conversation, DirectoryEntry};

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::person_avatar;

/// Desktop's one-time alerts (`OneTimeModalState`): each shows at most
/// once while picking.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Alerts {
    maximum: bool,
    recommended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeAlert {
    Maximum,
    Recommended,
}

/// The group size limits, counting us (see `RemoteConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    recommended: usize,
    hard: usize,
}

impl Limits {
    fn now() -> Self {
        let config = signal_backend::remote_config();
        Self { recommended: config.group_size_recommended(), hard: config.group_size_hard_limit() }
    }
}

/// Desktop's `toggleSelectedContactForGroupAddition`
/// (`ts/groups/toggleSelectedContactForGroupAddition.std.ts`): picks or
/// unpicks `sid`. A pick that would take the group past the hard limit
/// doesn't happen; one that reaches it, or the recommended limit, brings
/// that alert the first time. `already` is how many are in the group,
/// counting us (1 for a new group).
fn toggle(chosen: &mut BTreeSet<String>, sid: &str, already: usize, limits: Limits, alerts: &mut Alerts) -> Option<SizeAlert> {
    if chosen.remove(sid) {
        return None;
    }
    let expected = chosen.len() + already + 1;
    if expected > limits.hard {
        return None;
    }
    chosen.insert(sid.to_owned());
    if expected == limits.hard && !alerts.maximum {
        alerts.maximum = true;
        return Some(SizeAlert::Maximum);
    }
    if expected >= limits.recommended && !alerts.recommended {
        alerts.recommended = true;
        return Some(SizeAlert::Recommended);
    }
    None
}

/// Desktop's group size alerts (`AddGroupMemberErrorDialog.dom.tsx`), in
/// its words; `max` as each caller of Desktop's passes it.
fn show_alert(alert: SizeAlert, max: usize, window: &mut Window, cx: &mut App) {
    let (title, description) = match alert {
        SizeAlert::Maximum => ("Maximum group size reached", format!("Signal groups can have a maximum of {max} members.")),
        SizeAlert::Recommended => (
            "Recommended member limit reached",
            format!(
                "Signal groups perform best with {max} members or less. Adding more members will cause delays sending and receiving messages."
            ),
        ),
    };
    window.open_alert_dialog(cx, move |alert, _, _| alert.title(title).description(SharedString::from(description.clone())));
}

struct Picking {
    people: Vec<DirectoryEntry>,
    chosen: BTreeSet<String>,
    alerts: Alerts,
}

/// Who's already in the group (ACIs), when adding to one.
struct Adding {
    conversation: Conversation,
}

pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let _ = open_picker(state, None, window, cx);
}

/// Desktop's "Add members" for a group we may add people to.
pub fn open_add_members(state: Entity<AppState>, conversation: Conversation, window: &mut Window, cx: &mut App) {
    let _ = open_picker(state, Some(Adding { conversation }), window, cx);
}

fn open_picker(state: Entity<AppState>, adding: Option<Adding>, window: &mut Window, cx: &mut App) -> Entity<Picking> {
    let limits = Limits::now();
    // A new group has us in it.
    let already = adding.as_ref().map_or(1, |a| a.conversation.members.len());
    let existing: BTreeSet<String> = adding.as_ref().map(|a| a.conversation.members.iter().cloned().collect()).unwrap_or_default();
    // Desktop's `AddGroupMembersModal`: a full group says so at once; one
    // past the recommended size doesn't say so again.
    let alerts = Alerts { maximum: already >= limits.hard, recommended: already >= limits.recommended };
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Group name (required)"));
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search name or number"));
    let picking = cx.new(|_| Picking { people: Vec::new(), chosen: BTreeSet::new(), alerts });
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let query = backend.spawn(async move { db.people().await });
    let loaded = picking.clone();
    cx.spawn(async move |cx| {
        let people = query.await.unwrap_or_default();
        loaded.update(cx, |p, cx| {
            p.people = people;
            cx.notify();
        });
    })
    .detach();
    // What Desktop's alerts count: a new group's leave us out
    // (`LeftPaneChooseGroupMembersHelper`), "Add members" doesn't.
    let is_adding = adding.is_some();
    let alert_max = move |alert: SizeAlert| match (alert, is_adding) {
        (SizeAlert::Maximum, false) => limits.hard - 1,
        (SizeAlert::Recommended, false) => limits.recommended - 1,
        (SizeAlert::Maximum, true) => limits.hard,
        (SizeAlert::Recommended, true) => limits.recommended,
    };
    let adding_to = adding.map(|a| a.conversation);
    let title = if adding_to.is_some() { "Add members" } else { "New group" };
    let picked = picking.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let needle = search.read(cx).value().trim().to_lowercase();
        let chosen = picking.read(cx).chosen.clone();
        // Desktop's `hasSelectedMaximumNumberOfContacts` (new group) and
        // `ChooseGroupMembersModal` (adding): the group would be full.
        let full = chosen.len() + already >= limits.hard;
        let people: Vec<DirectoryEntry> = picking
            .read(cx)
            .people
            .iter()
            .filter(|p| needle.is_empty() || p.title.to_lowercase().contains(&needle) || p.phone.as_deref().is_some_and(|ph| ph.contains(&needle)))
            .cloned()
            .collect();
        let rows = people.into_iter().map(|person| {
            let sid = person.id.as_direct().unwrap_or_default().to_owned();
            let member = existing.contains(&sid);
            let on = member || chosen.contains(&sid);
            // Desktop's `ContactCheckboxDisabledReason`: already added, or
            // the group is full.
            let disabled = member || (!on && full);
            let picking = picking.clone();
            let toggled = sid.clone();
            let selector = format!("pick-{sid}");
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector)
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .when(disabled, |d| d.opacity(0.5))
                .when(!disabled, |d| {
                    d.cursor_pointer().hover(|d| d.bg(palette.row_hover)).on_click(move |_, window, cx| {
                        let alert = picking.update(cx, |p, _| {
                            let mut alerts = p.alerts;
                            let alert = toggle(&mut p.chosen, &toggled, already, limits, &mut alerts);
                            p.alerts = alerts;
                            alert
                        });
                        if let Some(alert) = alert {
                            show_alert(alert, alert_max(alert), window, cx);
                        }
                        window.refresh();
                    })
                })
                .child(person_avatar(state.read(cx).photo_of(&sid), &person.title, color_for(&person.id.0), px(32.)))
                .child(div().flex_1().truncate().child(person.title.clone()))
                .when(member, |d| d.child(div().text_xs().text_color(palette.text_secondary).child("Already a member")))
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .border_2()
                        .border_color(if on { palette.accent } else { palette.text_secondary })
                        .when(on, |d| d.bg(palette.accent)),
                )
        });
        let (create_state, create_name, create_picking) = (state.clone(), name.clone(), picking.clone());
        let ready = !chosen.is_empty() && (adding_to.is_some() || !name.read(cx).value().trim().is_empty());
        let target = adding_to.clone();
        let footer = DialogFooter::new()
            .child(Button::new("new-group-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
            .child(
                Button::new("new-group-create")
                    .label(if target.is_some() { "Update" } else { "Create" })
                    .primary()
                    .disabled(!ready)
                    .on_click(move |_, window, cx| {
                        let members: Vec<String> = create_picking.read(cx).chosen.iter().cloned().collect();
                        match &target {
                            Some(group) => confirm_additions(create_state.clone(), group.clone(), members, window, cx),
                            None => {
                                let title = create_name.read(cx).value().trim().to_string();
                                create_state.read(cx).send(Command::CreateGroup { title, members });
                                window.close_all_dialogs(cx);
                            }
                        }
                    }),
            );
        dialog
            .title(title)
            .w(px(440.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(adding_to.is_none(), |d| d.child(Input::new(&name)))
                    .child(Input::new(&search))
                    .child(div().text_xs().text_color(palette.text_secondary).child(format!("{} chosen", chosen.len())))
                    .child(div().id("new-group-people").h(px(320.)).overflow_y_scroll().children(rows)),
            )
            .footer(footer)
    });
    if is_adding && already >= limits.hard {
        show_alert(SizeAlert::Maximum, limits.hard, window, cx);
    }
    picked
}

/// Desktop's `ConfirmAdditionsModal`: "Add Alice to "Hikers"?".
fn confirm_additions(state: Entity<AppState>, group: Conversation, members: Vec<String>, window: &mut Window, cx: &mut App) {
    let one = members.len() == 1;
    let title = if one {
        let name = state.read(cx).conversation(&signal_store::ConversationId::direct(&members[0])).map(|c| c.title.clone());
        format!("Add {} to \"{}\"?", name.unwrap_or_else(|| "this person".into()), group.title)
    } else {
        format!("Add {} members to \"{}\"?", members.len(), group.title)
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, conversation, members) = (state.clone(), group.id.clone(), members.clone());
        alert
            .title(SharedString::from(title.clone()))
            .confirm()
            .ok_text(if one { "Add member" } else { "Add members" })
            .on_ok(move |_, window, cx| {
                state.read(cx).send(Command::AddGroupMembers { conversation: conversation.clone(), members: members.clone() });
                window.close_all_dialogs(cx);
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits { recommended: 4, hard: 6 };

    fn pick(chosen: &mut BTreeSet<String>, sid: &str, already: usize, alerts: &mut Alerts) -> Option<SizeAlert> {
        toggle(chosen, sid, already, LIMITS, alerts)
    }

    #[test]
    fn picking_stops_at_the_hard_limit_with_desktops_alerts() {
        let (mut chosen, mut alerts) = (BTreeSet::new(), Alerts::default());
        assert_eq!(pick(&mut chosen, "a", 1, &mut alerts), None);
        assert_eq!(pick(&mut chosen, "b", 1, &mut alerts), None);
        // Us and three: the recommended size.
        assert_eq!(pick(&mut chosen, "c", 1, &mut alerts), Some(SizeAlert::Recommended));
        assert_eq!(pick(&mut chosen, "d", 1, &mut alerts), None, "once");
        assert_eq!(pick(&mut chosen, "e", 1, &mut alerts), Some(SizeAlert::Maximum));
        // Full: no more picks, but unpicking works.
        assert_eq!(pick(&mut chosen, "f", 1, &mut alerts), None);
        assert!(!chosen.contains("f"));
        assert_eq!(pick(&mut chosen, "e", 1, &mut alerts), None);
        assert!(!chosen.contains("e"));
        assert_eq!(pick(&mut chosen, "e", 1, &mut alerts), None, "the maximum alert is once too");
        assert_eq!(chosen.len(), 5);
    }

    #[gpui_kit::test]
    fn adding_members_picks_people_not_in_the_group(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{Modifiers, VisualTestContext, WindowBounds, WindowOptions, size};
        use signal_backend::{Backend, Config};
        use signal_store::ConversationId;
        const HIKERS: &str = "g:0000000000000000000000000000000000000000000000000000000000000001";
        const MAYA: &str = "00000000-0000-4000-8000-00000000a11c";
        const ANA: &str = "00000000-0000-4000-8000-0000000000f0";
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
        let mut cx = VisualTestContext::from_window(window, cx);
        let wait = |cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            for _ in 0..300 {
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out waiting for {what}");
        };
        let hikers = ConversationId(HIKERS.into());
        wait(&mut cx, "the demo group", &|cx| state.read_with(cx, |s, _| s.conversation(&hikers).is_some_and(|c| c.can_add_members)));
        let group = state.read_with(&cx, |s, _| s.conversation(&hikers).cloned()).unwrap();
        let picking = cx.update(|window, cx| open_picker(state.clone(), Some(Adding { conversation: group }), window, cx));
        let row = |who: &str| -> &'static str { Box::leak(format!("pick-{who}").into_boxed_str()) };
        wait(&mut cx, "the people", &|cx| cx.debug_bounds(row(ANA)).is_some() && cx.debug_bounds(row(MAYA)).is_some());
        // Members are there, but can't be picked again.
        let at = cx.debug_bounds(row(MAYA)).unwrap().center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
        assert!(picking.read_with(&cx, |p, _| p.chosen.is_empty()));
        let at = cx.debug_bounds(row(ANA)).unwrap().center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(picking.read_with(&cx, |p, _| p.chosen.iter().cloned().collect::<Vec<_>>()), vec![ANA.to_owned()]);

        // What "Add member" sends adds her.
        state.read_with(&cx, |s, _| s.send(Command::AddGroupMembers { conversation: hikers.clone(), members: vec![ANA.into()] }));
        wait(&mut cx, "her to be added", &|cx| {
            state.read_with(cx, |s, _| s.conversation(&hikers).is_some_and(|c| c.members.iter().any(|m| m == ANA)))
        });
        backend.shutdown();
    }

    #[test]
    fn people_already_in_the_group_count() {
        let (mut chosen, mut alerts) = (BTreeSet::new(), Alerts::default());
        assert_eq!(pick(&mut chosen, "a", 5, &mut alerts), Some(SizeAlert::Maximum));
        assert_eq!(pick(&mut chosen, "b", 5, &mut alerts), None);
        assert_eq!(chosen.len(), 1);
    }
}
