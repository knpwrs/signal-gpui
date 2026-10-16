//! The menu bar, as Signal Desktop's on macOS (`app/menu.std.ts` with its
//! `updateForMac`): the app menu, File, Edit, View, Window and Help, less
//! what this app doesn't have (Desktop's developer tools and "Force
//! update", and its Speech submenu).

use gpui_kit::component::input::{Copy, Cut, Delete, Paste, Redo, SelectAll, Undo};
use gpui_kit::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window};

use crate::shortcuts::ShowKeyboardShortcuts;
use crate::{About, CreateStickerPack, OpenSettings, Quit, ShowDebugLog};

gpui_kit::actions!(
    signal,
    [
        ResetZoom,
        ZoomIn,
        ZoomOut,
        ToggleFullScreen,
        Minimize,
        ZoomWindow,
        ShowWindow,
        BringAllToFront,
        CloseWindow,
        Hide,
        HideOthers,
        ShowAll,
        PasteAndMatchStyle,
        ContactUs,
        GoToForums,
        GoToSupportPage,
    ]
);

/// Desktop's "Go to Forums" and "Go to Support Page" (`openForums`,
/// `openSupportPage` in app/main.main.ts). Its "Contact Us" opens Signal's
/// support form for Signal Desktop, which doesn't support this app, so it
/// opens this project's issues instead.
const FORUMS_URL: &str = "https://community.signalusers.org/";
const SUPPORT_URL: &str = "https://support.signal.org/hc/sections/360001602812";
const CONTACT_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Desktop's accelerators.
        KeyBinding::new("cmd-0", ResetZoom, None),
        KeyBinding::new("cmd-=", ZoomIn, None),
        KeyBinding::new("cmd-+", ZoomIn, None),
        KeyBinding::new("cmd--", ZoomOut, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-shift-0", ShowWindow, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
    ]);
}

/// The menus, and the app-wide handlers for their own actions (the rest
/// are handled where they apply: Edit's by the focused text field, Settings
/// and the like by the main window).
pub fn install(cx: &mut App) {
    cx.set_menus(vec![
        Menu::new("Signal GPUI").items([
            MenuItem::action("About Signal GPUI", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Signal GPUI", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Signal GPUI", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Create/upload sticker pack", CreateStickerPack),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            // Text fields here hold plain text, so this is Paste.
            MenuItem::action("Paste and Match Style", PasteAndMatchStyle),
            MenuItem::action("Delete", Delete),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Actual Size", ResetZoom),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::separator(),
            MenuItem::action("Toggle Full Screen", ToggleFullScreen),
            MenuItem::separator(),
            MenuItem::action("Debug Log", ShowDebugLog),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", ZoomWindow),
            MenuItem::action("Show", ShowWindow),
            MenuItem::separator(),
            MenuItem::action("Bring All to Front", BringAllToFront),
        ]),
        Menu::new("Help").items([
            MenuItem::action("Show Keyboard Shortcuts", ShowKeyboardShortcuts),
            MenuItem::separator(),
            MenuItem::action("Contact Us", ContactUs),
            MenuItem::action("Go to Forums", GoToForums),
            MenuItem::action("Go to Support Page", GoToSupportPage),
        ]),
    ]);

    cx.on_action(|_: &ResetZoom, cx| zoom(cx, None));
    cx.on_action(|_: &ZoomIn, cx| zoom(cx, Some(1)));
    cx.on_action(|_: &ZoomOut, cx| zoom(cx, Some(-1)));
    cx.on_action(|_: &ToggleFullScreen, cx| with_active_window(cx, |window, _| window.toggle_fullscreen()));
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window, _| window.minimize_window()));
    cx.on_action(|_: &ZoomWindow, cx| with_active_window(cx, |window, _| window.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, crate::main_window::close));
    cx.on_action(|_: &ShowWindow, cx| crate::main_window::show(cx));
    cx.on_action(|_: &BringAllToFront, cx| {
        cx.activate(true);
        cx.defer(|cx| {
            for window in cx.windows() {
                let _ = window.update(cx, |_, window, _| window.activate_window());
            }
        });
    });
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &PasteAndMatchStyle, cx| {
        with_active_window(cx, |window, cx| window.dispatch_action(Box::new(Paste), cx))
    });
    // From the menu with the main window hidden or nothing in it focused
    // (otherwise the window handles them): the window comes back with them.
    cx.on_action(|_: &ShowKeyboardShortcuts, cx| {
        crate::main_window::show(cx);
        crate::main_window::update(cx, crate::views::shortcut_guide::open);
    });
    cx.on_action(|_: &About, cx| {
        crate::main_window::show(cx);
        crate::main_window::update(cx, crate::views::about::open);
    });
    cx.on_action(|_: &ShowDebugLog, cx| {
        let Some(state) = crate::main_window::state(cx) else { return };
        crate::main_window::show(cx);
        crate::main_window::update(cx, move |window, cx| crate::views::debug_log::open(state, window, cx));
    });
    cx.on_action(|_: &OpenSettings, cx| {
        let Some(state) = crate::main_window::state(cx) else { return };
        let ready = {
            let state = state.read(cx);
            matches!(state.screen, crate::state::Screen::Main) && !state.is_loading()
        };
        if ready {
            crate::main_window::show(cx);
            crate::main_window::update(cx, move |window, cx| crate::views::settings::open(state, window, cx));
        }
    });
    cx.on_action(|_: &ContactUs, cx| cx.open_url(CONTACT_URL));
    cx.on_action(|_: &GoToForums, cx| cx.open_url(FORUMS_URL));
    cx.on_action(|_: &GoToSupportPage, cx| cx.open_url(SUPPORT_URL));
}

/// Runs `f` on the window in front, once the action that asked is done
/// with it (a shortcut's action runs while its window is being updated).
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
    cx.defer(move |cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, cx| f(window, cx));
        }
    });
}

/// Desktop's Actual Size, Zoom In and Zoom Out (`zoomReset`, `zoomIn`,
/// `zoomOut`): the same zoom as Settings' Zoom level, saved with it.
fn zoom(cx: &mut App, levels: Option<i32>) {
    let Some(state) = crate::main_window::state(cx) else { return };
    state.update(cx, |state, cx| {
        let zoom = match levels {
            Some(levels) => crate::prefs::zoom_step(state.prefs.zoom, levels),
            None => 1.,
        };
        if (zoom - state.prefs.zoom).abs() > f32::EPSILON {
            state.set_zoom(zoom, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::*;
    use crate::views::test_support::wait;

    #[gpui_kit::test]
    fn view_menu_zooms_as_settings_does_and_close_window_hides(cx: &mut TestAppContext) {
        let (backend, state, window, _dir) = crate::main_window::tests::open_demo(cx);
        cx.update(install);
        let mut vcx = gpui_kit::VisualTestContext::from_window(window, cx);
        let vcx = &mut vcx;
        wait(vcx, "the account", &mut |cx| state.read_with(cx, |s, _| s.account.is_some()));
        let zoom = |cx: &mut gpui_kit::VisualTestContext| state.read_with(cx, |s, _| s.prefs.zoom);
        vcx.dispatch_action(ZoomIn);
        assert_eq!(zoom(vcx), 1.2);
        vcx.dispatch_action(ZoomIn);
        vcx.dispatch_action(ZoomOut);
        assert_eq!(zoom(vcx), 1.2);
        vcx.dispatch_action(ResetZoom);
        assert_eq!(zoom(vcx), 1.);
        // ⌘W on the main window hides it rather than closing it.
        vcx.dispatch_action(CloseWindow);
        vcx.run_until_parked();
        assert!(vcx.update(|_, cx| cx.windows().contains(&window)));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn menu_items_reach_a_hidden_window(cx: &mut TestAppContext) {
        let (backend, _state, window, _dir) = crate::main_window::tests::open_demo(cx);
        cx.update(install);
        // As macOS sends a menu item with no window in front.
        cx.update(|cx| cx.dispatch_action(&crate::shortcuts::ShowKeyboardShortcuts));
        let mut vcx = gpui_kit::VisualTestContext::from_window(window, cx);
        assert!(crate::views::test_support::shows(&mut vcx, "shortcut-guide"));
        backend.shutdown();
    }
}
