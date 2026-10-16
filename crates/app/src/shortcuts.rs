//! Signal Desktop's keyboard shortcuts, and the list its shortcut guide
//! shows (`ts/components/ShortcutGuide.dom.tsx`). The keys are Desktop's
//! (`ts/services/addGlobalKeyboardShortcuts.preload.ts`, `LeftPane`,
//! `CompositionArea`, `ConversationHeader`, `IncomingCallBar`,
//! `CallScreen`, `CallingLobby`); each is handled where it applies (see
//! `views::root` and `views::call`).
//!
//! Desktop's that aren't here: "Navigate by section" (⌘T), "Open chat menu"
//! (⌘⇧L), "Open context menu for selected message" (⌘F12), "Expand
//! composer" (⌘⇧K) and sending in it (⌘Enter), and "Toggle expanded
//! preview" in a call (⇧P). See todo.md.

use gpui_kit::{App, KeyBinding};

gpui_kit::actions!(
    signal,
    [
        ShowKeyboardShortcuts,
        PreviousUnreadConversation,
        NextUnreadConversation,
        StartNewChat,
        FocusComposer,
        OpenAllMedia,
        OpenEmojiChooser,
        OpenStickerChooser,
        OpenGifChooser,
        ArchiveChat,
        UnarchiveChat,
        CloseChat,
        AttachFile,
        RemoveLinkPreview,
        RemoveDraftAttachments,
        StartAudioCall,
        StartVideoCall,
        AcceptVideoCall,
        AcceptAudioCall,
        DeclineCall,
        HangUp,
        ToggleCallAudio,
        ToggleCallVideo,
        ToggleRaiseHand,
    ]
);

/// ⌘1 to ⌘9: the chat at that place in the list (0 for the first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, gpui_kit::Action)]
#[action(namespace = signal, no_json)]
pub struct JumpToChat(pub usize);

/// Key context of the main window's content.
const MAIN: Option<&str> = Some("Signal");
/// Key context of the full-window call screen and lobby.
pub const CALL_SCREEN: &str = "CallScreen";

pub fn bind_keys(cx: &mut App) {
    let mut bindings = vec![
        KeyBinding::new("cmd-/", ShowKeyboardShortcuts, None),
        KeyBinding::new("alt-shift-up", PreviousUnreadConversation, MAIN),
        KeyBinding::new("alt-shift-down", NextUnreadConversation, MAIN),
        KeyBinding::new("cmd-shift-[", crate::PreviousConversation, MAIN),
        KeyBinding::new("cmd-shift-]", crate::NextConversation, MAIN),
        KeyBinding::new("cmd-n", StartNewChat, MAIN),
        KeyBinding::new("cmd-shift-t", FocusComposer, MAIN),
        KeyBinding::new("cmd-shift-m", OpenAllMedia, MAIN),
        KeyBinding::new("cmd-shift-j", OpenEmojiChooser, MAIN),
        KeyBinding::new("cmd-shift-o", OpenStickerChooser, MAIN),
        KeyBinding::new("cmd-shift-g", OpenGifChooser, MAIN),
        KeyBinding::new("cmd-shift-a", ArchiveChat, MAIN),
        KeyBinding::new("cmd-shift-u", UnarchiveChat, MAIN),
        KeyBinding::new("cmd-shift-c", CloseChat, MAIN),
        KeyBinding::new("cmd-u", AttachFile, MAIN),
        KeyBinding::new("cmd-p", RemoveLinkPreview, MAIN),
        KeyBinding::new("cmd-shift-p", RemoveDraftAttachments, MAIN),
        // Desktop's `ControlOrAlt` is Control on macOS.
        KeyBinding::new("ctrl-shift-c", StartAudioCall, MAIN),
        KeyBinding::new("ctrl-shift-y", StartVideoCall, MAIN),
        KeyBinding::new("ctrl-shift-v", AcceptVideoCall, MAIN),
        KeyBinding::new("ctrl-shift-a", AcceptAudioCall, MAIN),
        KeyBinding::new("ctrl-shift-d", DeclineCall, MAIN),
        KeyBinding::new("ctrl-shift-e", HangUp, MAIN),
        // Text fields select to the line's start and end with these; while
        // a call rings or runs they answer and hang up instead, as in
        // Desktop (whose shortcuts work in text fields too). Added after
        // the text fields' own, so they're tried first.
        KeyBinding::new("ctrl-shift-a", AcceptAudioCall, Some("Input")),
        KeyBinding::new("ctrl-shift-e", HangUp, Some("Input")),
        KeyBinding::new("shift-m", ToggleCallAudio, Some(CALL_SCREEN)),
        KeyBinding::new("shift-v", ToggleCallVideo, Some(CALL_SCREEN)),
        KeyBinding::new("shift-h", ToggleRaiseHand, Some(CALL_SCREEN)),
    ];
    for n in 1..=9 {
        bindings.push(KeyBinding::new(&format!("cmd-{n}"), JumpToChat(n - 1), MAIN));
    }
    cx.bind_keys(bindings);
}

/// One line of the guide: what it does, and its keys (each a set pressed
/// together; some have more than one set).
pub struct Shortcut {
    pub description: &'static str,
    pub keys: &'static [&'static [&'static str]],
}

const fn s(description: &'static str, keys: &'static [&'static [&'static str]]) -> Shortcut {
    Shortcut { description, keys }
}

/// Desktop's guide, in its order and words, with its macOS key labels;
/// only the shortcuts this app has.
pub const GUIDE: &[(&str, &[Shortcut])] = &[
    (
        "Navigation",
        &[
            s("Previous chat", &[&["Option", "↑"], &["Ctrl", "Shift", "Tab"]]),
            s("Next chat", &[&["Option", "↓"], &["Ctrl", "Tab"]]),
            s("Previous unread chat", &[&["Option", "Shift", "↑"]]),
            s("Next unread chat", &[&["Option", "Shift", "↓"]]),
            s("Jump to chat", &[&["⌘", "1 to 9"]]),
            s("Preferences", &[&["⌘", ","]]),
            s("Start new chat", &[&["⌘", "N"]]),
            s("Search", &[&["⌘", "F"]]),
            s("Search in chat", &[&["⌘", "Shift", "F"]]),
            s("Focus composer", &[&["⌘", "Shift", "T"]]),
            s("Open All Media view", &[&["⌘", "Shift", "M"]]),
            s("Open emoji chooser", &[&["⌘", "Shift", "J"]]),
            s("Open sticker chooser", &[&["⌘", "Shift", "O"]]),
            s("Open GIF chooser", &[&["⌘", "Shift", "G"]]),
            s("Begin recording voice note", &[&["⌘", "Shift", "Y"]]),
            s("Archive chat", &[&["⌘", "Shift", "A"]]),
            s("Unarchive chat", &[&["⌘", "Shift", "U"]]),
            s("Close current chat", &[&["⌘", "Shift", "C"]]),
        ],
    ),
    (
        "Composer",
        &[
            s("Add newline to message", &[&["Shift", "Enter"]]),
            s("Attach file", &[&["⌘", "U"]]),
            s("Remove draft link preview", &[&["⌘", "P"]]),
            s("Remove all draft attachments", &[&["⌘", "Shift", "P"]]),
            s("Edit the previous message", &[&["↑"]]),
            s("Mark selected text as bold", &[&["⌘", "B"]]),
            s("Mark selected text as italic", &[&["⌘", "I"]]),
            s("Mark selected text as strikethrough", &[&["⌘", "Shift", "X"]]),
            s("Mark selected text as monospace", &[&["⌘", "E"]]),
            s("Mark selected text as a spoiler", &[&["⌘", "Shift", "B"]]),
        ],
    ),
    (
        "Calling",
        &[
            s("Toggle mute on and off", &[&["Shift", "M"]]),
            s("Toggle video on and off", &[&["Shift", "V"]]),
            s("Raise or lower your hand (group calls only)", &[&["Shift", "H"]]),
            s("Answer call with video (video calls only)", &[&["Ctrl", "Shift", "V"]]),
            s("Answer call without video", &[&["Ctrl", "Shift", "A"]]),
            s("Start voice call", &[&["Ctrl", "Shift", "C"]]),
            s("Start video call", &[&["Ctrl", "Shift", "Y"]]),
            s("Decline call", &[&["Ctrl", "Shift", "D"]]),
            s("End call", &[&["Ctrl", "Shift", "E"]]),
        ],
    ),
];

#[cfg(test)]
mod tests {
    use gpui_kit::{TestAppContext, VisualTestContext};
    use signal_store::ConversationId;

    use crate::state::AppState;
    use crate::views::test_support::{shows, wait};

    const MAYA: &str = "d:00000000-0000-4000-8000-00000000a11c";

    fn selected(cx: &mut VisualTestContext, state: &gpui_kit::Entity<AppState>) -> Option<String> {
        state.read_with(cx, |s, _| s.selected.as_ref().map(|c| c.0.clone()))
    }

    #[gpui_kit::test]
    fn desktops_shortcuts_reach_the_chat_list_composer_and_calls(cx: &mut TestAppContext) {
        let (backend, state, window, _dir) = crate::main_window::tests::open_demo(cx);
        cx.update(super::bind_keys);
        let mut vcx = VisualTestContext::from_window(window, cx);
        let cx = &mut vcx;
        wait(cx, "the chats", &mut |cx| state.read_with(cx, |s, _| s.conversations.len() >= 4));
        // A chat open, so its composer has the keyboard.
        let first = state.read_with(cx, |s, _| s.conversations[0].id.clone());
        state.update(cx, |s, cx| s.select(Some(first), cx));
        cx.run_until_parked();
        let ids: Vec<String> = state.read_with(cx, |s, _| s.conversations.iter().map(|c| c.id.0.clone()).collect());

        // ⌘2: the second chat in the list; ⌘9 past the end: the last.
        cx.simulate_keystrokes("cmd-2");
        wait(cx, "the second chat", &mut |cx| selected(cx, &state).as_deref() == Some(ids[1].as_str()));
        cx.simulate_keystrokes("cmd-9");
        let last = if ids.len() >= 9 { ids[8].clone() } else { ids.last().unwrap().clone() };
        wait(cx, "the ninth or last chat", &mut |cx| selected(cx, &state) == Some(last.clone()));
        // ⌥⇧↑: the nearest unread chat above.
        let unread: Vec<bool> =
            state.read_with(cx, |s, _| s.conversations.iter().map(|c| c.unread_count > 0 || c.marked_unread).collect());
        let at = ids.iter().position(|id| *id == last).unwrap();
        let want = crate::state::conversation_in_direction(&unread, Some(at), true, true).expect("an unread chat in the demo");
        cx.simulate_keystrokes("alt-shift-up");
        wait(cx, "the unread chat", &mut |cx| selected(cx, &state).as_deref() == Some(ids[want].as_str()));

        // ⌘⇧A archives the open chat; ⌘⇧U brings it back.
        let maya = ConversationId(MAYA.into());
        state.update(cx, |s, cx| s.select(Some(maya.clone()), cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-a");
        wait(cx, "archived", &mut |cx| state.read_with(cx, |s, _| s.conversation(&maya).is_some_and(|c| c.archived)));
        cx.simulate_keystrokes("cmd-shift-u");
        wait(cx, "back in the inbox", &mut |cx| state.read_with(cx, |s, _| s.conversation(&maya).is_some_and(|c| !c.archived)));

        // ⌘/ shows the guide.
        cx.simulate_keystrokes("cmd-/");
        assert!(shows(cx, "shortcut-guide"));
        cx.update(|window, cx| {
            use gpui_kit::component::WindowExt as _;
            window.close_all_dialogs(cx)
        });

        // Ctrl-Shift-C: the lobby for a voice call, which takes the
        // keyboard; ⇧M there mutes.
        state.update(cx, |s, cx| s.select(Some(maya.clone()), cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-shift-c");
        wait(cx, "the lobby", &mut |cx| state.read_with(cx, |s, _| s.lobby.as_ref().is_some_and(|l| !l.video && l.audio)));
        cx.simulate_keystrokes("shift-m");
        wait(cx, "muted", &mut |cx| state.read_with(cx, |s, _| s.lobby.as_ref().is_some_and(|l| !l.audio)));
        state.update(cx, |s, cx| s.close_call_lobby(cx));
        cx.run_until_parked();

        // ⌘⇧C closes the chat (the keyboard is back with it).
        cx.simulate_keystrokes("cmd-shift-c");
        wait(cx, "no chat open", &mut |cx| selected(cx, &state).is_none());
        backend.shutdown();
    }
}
