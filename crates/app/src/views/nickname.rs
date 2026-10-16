//! Nicknames and notes, as Signal Desktop has them: the "Nickname" dialog
//! (`ts/components/EditNicknameAndNoteModal.dom.tsx`), the note's preview
//! (`NotePreviewModal.dom.tsx`) and "Delete nickname?" (the
//! `ConfirmDeleteNicknameAndNote` dialog in `ConversationDetails.dom.tsx`).

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, SharedString, Subscription, Window, div, px};
use signal_backend::Command;
use signal_store::{ContactNickname, Conversation, ConversationId, ConversationKind, NICKNAME_LIMITS, remaining_count, truncate_text};

use crate::state::AppState;
use crate::theme::Palette;
use crate::views::common::conversation_avatar;

/// Desktop's `canHaveNicknameAndNote`: a person, not a group, the Signal
/// chat or ourselves.
pub fn can_have_nickname(conversation: &Conversation) -> bool {
    conversation.kind == ConversationKind::Direct && !conversation.id.is_signal()
}

/// The three fields, cut to Desktop's limits as you type.
pub struct NicknameForm {
    pub given: Entity<InputState>,
    pub family: Entity<InputState>,
    pub note: Entity<TextareaState>,
    initial: ContactNickname,
    _subscriptions: Vec<Subscription>,
}

impl NicknameForm {
    pub fn new(initial: ContactNickname, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let limits = NICKNAME_LIMITS;
        let name_field = |placeholder: &'static str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let value = value.to_owned();
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value));
            let subscription = cx.subscribe_in(&input, window, move |_, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    let value = input.read(cx).value().to_string();
                    let cut = truncate_text(&value, limits.name_graphemes, limits.name_bytes);
                    if cut.len() < value.len() {
                        let cut = cut.to_owned();
                        input.update(cx, |input, cx| input.set_value(cut, window, cx));
                    }
                    cx.notify();
                }
            });
            (input, subscription)
        };
        let (given, given_sub) = name_field("First name", &initial.given, window, cx);
        let (family, family_sub) = name_field("Last name", &initial.family, window, cx);
        let note_value = initial.note.clone();
        let note = cx.new(|cx| {
            let mut note = TextareaState::new(window, cx).auto_grow(2, 5).placeholder("Note");
            if !note_value.is_empty() {
                note.set_value(note_value, window, cx);
            }
            note
        });
        let note_sub = cx.subscribe_in(&note, window, move |_, input, event: &InputEvent, window, cx| {
            if let InputEvent::Change = event {
                let value = input.read(cx).value().to_string();
                let cut = truncate_text(&value, limits.note_graphemes, limits.note_bytes);
                if cut.len() < value.len() {
                    let cut = cut.to_owned();
                    input.update(cx, |input, cx| input.set_value(cut, window, cx));
                }
                cx.notify();
            }
        });
        Self { given, family, note, initial, _subscriptions: vec![given_sub, family_sub, note_sub] }
    }

    /// What Save would save: trimmed (Desktop's `toOptionalStringValue`).
    pub fn result(&self, cx: &App) -> ContactNickname {
        signal_store::limit_nickname(&ContactNickname {
            given: self.given.read(cx).value().to_string(),
            family: self.family.read(cx).value().to_string(),
            note: self.note.read(cx).value().to_string(),
            plain_name: None,
        })
    }

    /// Desktop enables Save once something differs from what was there.
    pub fn changed(&self, cx: &App) -> bool {
        let now = self.result(cx);
        (now.given.as_str(), now.family.as_str(), now.note.as_str())
            != (self.initial.given.as_str(), self.initial.family.as_str(), self.initial.note.as_str())
    }
}

impl Render for NicknameForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let limits = NICKNAME_LIMITS;
        let count = |text: &str, graphemes, bytes| {
            remaining_count(text, graphemes, bytes).map(|(left, warn)| {
                div().text_xs().text_color(if warn { palette.danger } else { palette.text_secondary }).child(left.to_string())
            })
        };
        let name = |input: &Entity<InputState>, selector: &'static str, cx: &App| {
            let left = count(&input.read(cx).value(), limits.name_graphemes, limits.name_bytes);
            div().debug_selector(move || selector.into()).child(Input::new(input).when_some(left, |i, left| i.suffix(left)))
        };
        let note_left = count(&self.note.read(cx).value(), limits.note_graphemes, limits.note_bytes);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(name(&self.given, "nickname-given", cx))
            .child(name(&self.family, "nickname-family", cx))
            .child(
                div()
                    .debug_selector(|| "nickname-note-field".into())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Textarea::new(&self.note))
                    .when_some(note_left, |d, left| d.child(div().flex().justify_end().child(left))),
            )
    }
}

/// Desktop's "Nickname" dialog for `conversation`, filled with `current`.
pub fn open_edit(
    state: Entity<AppState>,
    conversation: Conversation,
    current: ContactNickname,
    window: &mut Window,
    cx: &mut App,
) -> Entity<NicknameForm> {
    let form = cx.new(|cx| NicknameForm::new(current, window, cx));
    let shown = form.clone();
    form.read(cx).given.clone().update(cx, |input, cx| input.focus(window, cx));
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let changed = form.read(cx).changed(cx);
        let (save_form, save_state, id) = (form.clone(), state.clone(), conversation.id.clone());
        dialog
            .title("Nickname")
            .w(px(400.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .text_color(palette.text_secondary)
                            .child("Nicknames & notes are stored with Signal and end-to-end encrypted. They are only visible to you."),
                    )
                    .child(div().flex().justify_center().child(conversation_avatar(&conversation, px(80.))))
                    .child(form.clone()),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("nickname-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("nickname-save").label("Save").primary().disabled(!changed).on_click(move |_, window, cx| {
                        if !save_form.read(cx).changed(cx) {
                            return;
                        }
                        let nickname = save_form.read(cx).result(cx);
                        save_state.read(cx).send(Command::SetNickname { conversation: id.clone(), nickname });
                        window.close_all_dialogs(cx);
                    })),
            )
    });
    shown
}

/// Desktop's note preview: the whole note, with Edit and Done.
pub fn open_note(state: Entity<AppState>, conversation: Conversation, current: ContactNickname, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, _| {
        let (state, chat, nickname) = (state.clone(), conversation.clone(), current.clone());
        dialog
            .title("Note")
            .w(px(400.))
            .child(div().debug_selector(|| "note-preview".into()).text_sm().child(SharedString::from(current.note.clone())))
            .footer(
                DialogFooter::new()
                    .child(Button::new("note-edit").label("Edit").outline().on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        open_edit(state.clone(), chat.clone(), nickname.clone(), window, cx);
                    }))
                    .child(Button::new("note-done").label("Done").primary().on_click(|_, window, cx| window.close_all_dialogs(cx))),
            )
    });
}

/// Desktop's "Delete nickname?": deletes the nickname and the note.
pub fn confirm_delete(state: Entity<AppState>, conversation: ConversationId, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, conversation) = (state.clone(), conversation.clone());
        alert
            .title("Delete nickname?")
            .description("This will permanently delete this nickname and note.")
            .confirm()
            .ok_text("Delete")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                state.read(cx).send(Command::SetNickname { conversation: conversation.clone(), nickname: ContactNickname::default() });
                window.close_all_dialogs(cx);
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext, WindowBounds, WindowOptions, px, size};
    use signal_backend::{Backend, Config};
    use signal_store::{ContactNickname, ConversationId};

    use super::*;

    /// The demo's Leo Martin (a nickname over "Leonardo Martin", and a note)
    /// and Maya Chen (neither).
    const LEO: &str = "d:00000000-0000-4000-8000-0000000000f1";
    const MAYA: &str = "d:00000000-0000-4000-8000-00000000a11c";

    fn wait(cx: &mut VisualTestContext, what: &str, done: &mut dyn FnMut(&mut VisualTestContext) -> bool) {
        for _ in 0..300 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn shows(cx: &mut VisualTestContext, what: &'static str) -> bool {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.debug_bounds(what).is_some()
    }

    /// Clicks a button by its id.
    fn press(cx: &mut VisualTestContext, id: &'static str) {
        for _ in 0..300 {
            let found = cx.update(|window, cx| {
                window.render_frame(cx);
                window.try_find(id).is_some()
            });
            if found {
                cx.update(|window, cx| window.click(id, cx));
                cx.run_until_parked();
                return;
            }
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("no button {id}");
    }

    /// Clicks the middle of an element marked with `debug_selector`.
    fn click_at(cx: &mut VisualTestContext, what: &'static str) {
        wait(cx, what, &mut |cx| cx.debug_bounds(what).is_some());
        let at = cx.debug_bounds(what).unwrap().center();
        cx.simulate_click(at, gpui_kit::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn nicknames_are_shown_edited_and_deleted(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        // As the app does: backend events into the model.
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
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(800.), px(900.)) })),
            ..Default::default()
        };
        let (window, _sidebar) = cx
            .update(|cx| {
                let state = state.clone();
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::sidebar::Sidebar::new(state, window, cx)))
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);
        let chat = |cx: &mut VisualTestContext, id: &str| {
            let id = ConversationId(id.into());
            let mut found = None;
            wait(cx, "the demo", &mut |cx| {
                state.update(cx, |s, cx| s.reload_conversations(cx));
                cx.run_until_parked();
                found = state.read_with(cx, |s, _| s.conversations.iter().find(|c| c.id == id).cloned());
                found.is_some()
            });
            found.unwrap()
        };
        let nickname_of = |id: &str| {
            let (db, sid) = (backend.db().clone(), id.trim_start_matches("d:").to_owned());
            futures::executor::block_on(backend.spawn(async move { db.contact_nickname(&sid).await })).unwrap()
        };
        let has_dialog = |cx: &mut VisualTestContext| cx.update(|window, cx| window.has_active_dialog(cx));

        // Leo's: "Leo Martin (Leonardo Martin)", and his note opens in full.
        // The demo saves names last.
        wait(cx, "the demo's names", &mut |_| nickname_of(LEO).is_some());
        let leo = chat(cx, LEO);
        assert_eq!(leo.title, "Leo Martin");
        cx.update(|window, cx| crate::views::chat_settings::open(state.clone(), leo, window, cx));
        wait(cx, "the nickname", &mut |cx| cx.debug_bounds("nickname-plain-name").is_some());
        press(cx, "nickname-delete");
        // That asked first; Cancel by closing it leaves Leo's nickname.
        cx.update(|window, cx| window.close_all_dialogs(cx));
        let leo = chat(cx, LEO);
        cx.update(|window, cx| crate::views::chat_settings::open(state.clone(), leo, window, cx));
        click_at(cx, "nickname-note");
        assert!(shows(cx, "note-preview"));
        press(cx, "note-done");
        assert!(!has_dialog(cx));
        assert!(nickname_of(LEO).is_some_and(|n| n.given == "Leo"));

        // Maya has none: only "Nickname", which opens an empty form whose
        // Save waits for a change.
        let maya = chat(cx, MAYA);
        cx.update(|window, cx| crate::views::chat_settings::open(state.clone(), maya.clone(), window, cx));
        press(cx, "nickname-edit");
        assert!(has_dialog(cx), "the Nickname dialog");
        cx.update(|window, cx| window.close_all_dialogs(cx));
        let form = cx.update(|window, cx| open_edit(state.clone(), maya.clone(), ContactNickname::default(), window, cx));
        cx.run_until_parked();
        // Save is enabled by `changed`; pressed before then, it does nothing.
        let changed = |cx: &mut VisualTestContext| form.read_with(cx, |f, cx| f.changed(cx));
        assert!(!changed(cx));
        press(cx, "nickname-save");
        assert!(has_dialog(cx), "nothing to save yet");
        cx.update(|window, cx| {
            let (given, note) = (form.read(cx).given.clone(), form.read(cx).note.clone());
            given.update(cx, |input, cx| input.set_value("  Mayday ", window, cx));
            note.update(cx, |input, cx| input.set_value("Plans the routes", window, cx));
        });
        cx.run_until_parked();
        assert!(changed(cx));
        press(cx, "nickname-save");

        // The chat is renamed, and the name she had is kept for later.
        wait(cx, "the new title", &mut |cx| {
            state.update(cx, |s, cx| s.reload_conversations(cx));
            state.read_with(cx, |s, _| s.conversations.iter().any(|c| c.id == maya.id && c.title == "Mayday"))
        });
        let saved = nickname_of(MAYA).unwrap();
        assert_eq!(
            (saved.given.as_str(), saved.family.as_str(), saved.note.as_str(), saved.plain_name.as_deref()),
            ("Mayday", "", "Plans the routes", Some("Maya Chen"))
        );

        // Deleted (after "Delete nickname?"): her name is back.
        let maya = chat(cx, MAYA);
        cx.update(|window, cx| crate::views::chat_settings::open(state.clone(), maya.clone(), window, cx));
        wait(cx, "her nickname", &mut |cx| cx.debug_bounds("nickname-plain-name").is_some());
        press(cx, "nickname-delete");
        press(cx, "ok");
        wait(cx, "the old title", &mut |cx| {
            state.update(cx, |s, cx| s.reload_conversations(cx));
            state.read_with(cx, |s, _| s.conversations.iter().any(|c| c.id == maya.id && c.title == "Maya Chen"))
        });
        assert_eq!(nickname_of(MAYA), None);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn the_form_stops_at_desktops_limits(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let current = ContactNickname { given: "Leo".into(), family: "Martin".into(), note: String::new(), plain_name: None };
        let window = cx.add_window(|window, cx| NicknameForm::new(current, window, cx));
        let form = window.root(cx).unwrap();
        let mut cx = VisualTestContext::from_window(*window, cx);
        assert!(!form.read_with(&cx, |f, cx| f.changed(cx)), "filled with what's there");
        let given = form.read_with(&cx, |f, _| f.given.clone());
        cx.update(|window, cx| given.update(cx, |input, cx| input.focus(window, cx)));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-a backspace");
        cx.simulate_input(&"x".repeat(30));
        cx.run_until_parked();
        assert_eq!(given.read_with(&cx, |input, _| input.value().len()), 26);
        let note = form.read_with(&cx, |f, _| f.note.clone());
        cx.update(|window, cx| note.update(cx, |input, cx| input.focus(window, cx)));
        cx.run_until_parked();
        cx.simulate_input(&"n".repeat(250));
        cx.run_until_parked();
        assert_eq!(note.read_with(&cx, |input, _| input.value().len()), 240);
        let result = form.read_with(&cx, |f, cx| f.result(cx));
        assert_eq!((result.given.len(), result.family.as_str(), result.note.len()), (26, "Martin", 240));
        assert!(form.read_with(&cx, |f, cx| f.changed(cx)));
    }

    #[test]
    fn only_people_get_nicknames() {
        let mut chat = Conversation {
            id: ConversationId(MAYA.into()),
            kind: ConversationKind::Direct,
            title: "Maya Chen".into(),
            phone: None,
            color: 0,
            members: vec![],
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
            expire_timer_version: 0,
            can_edit_info: true,
            left: false,
            description: None,
            admins: vec![],
            member_labels: Default::default(),
            can_label: false,
            avatar_path: None,
            last_outgoing_status: None,
        };
        assert!(can_have_nickname(&chat));
        chat.kind = ConversationKind::NoteToSelf;
        assert!(!can_have_nickname(&chat));
        chat.kind = ConversationKind::Group;
        assert!(!can_have_nickname(&chat));
        chat.kind = ConversationKind::Direct;
        chat.id = ConversationId::signal();
        assert!(!can_have_nickname(&chat));
    }
}
