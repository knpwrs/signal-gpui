//! Desktop's sticker manager (`ts/components/stickers/StickerManager.dom.tsx`,
//! `StickerManagerHeader`, `StickerManagerPackRow`) and the pack preview
//! (`StickerPreviewModal.dom.tsx`). Desktop shows the manager as a panel of
//! the open chat; here it's a dialog. Installed packs are reordered with
//! a pack's menu (Move up, Move down) where Desktop drags them.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, SharedString, TestSupportExt as _, Window, div, img, px};
use signal_backend::Command;
use signal_backend::stickers::StickerPackSummary;

use crate::state::{AppState, StickerPreview};
use crate::theme::Palette;

/// Desktop's manager tabs (`StickerManagerTabType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    All,
    MyStickers,
}

/// The installed packs' order after moving one up, and after moving it
/// down (each `None` where it can't move).
type Moves = (Option<Vec<Vec<u8>>>, Option<Vec<Vec<u8>>>);

const UNINSTALL_WARNING: &str = "You may not be able to re-install this sticker pack if you no longer have the source message.";

fn cover(pack: &StickerPackSummary, size: f32, palette: Palette) -> AnyElement {
    match &pack.cover {
        Some(path) => img(path.clone()).size(px(size)).flex_none().object_fit(gpui_kit::ObjectFit::Contain).into_any_element(),
        None => div().size(px(size)).flex_none().rounded_md().bg(palette.row_hover).into_any_element(),
    }
}

/// Desktop's uninstall: a blessed pack goes at once; another asks first.
fn uninstall(state: Entity<AppState>, pack: StickerPackSummary, window: &mut Window, cx: &mut App) {
    if pack.blessed {
        state.read(cx).send(Command::UninstallStickerPack { id: pack.id, key: pack.key });
        return;
    }
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (state, pack) = (state.clone(), pack.clone());
        alert.description(UNINSTALL_WARNING).confirm().ok_text("Uninstall").ok_variant(ButtonVariant::Danger).on_ok(move |_, _, cx| {
            state.read(cx).send(Command::UninstallStickerPack { id: pack.id.clone(), key: pack.key.clone() });
            true
        })
    });
}

/// One pack's row (`StickerManagerPackRow`): opens the pack; Install, or
/// Installed; Add or Remove on right-click; in My Stickers, moves.
fn pack_row(
    id: SharedString,
    pack: &StickerPackSummary,
    install_button: bool,
    moves: Option<Moves>,
    state: &Entity<AppState>,
    palette: Palette,
) -> AnyElement {
    let title = if pack.title.is_empty() { "Sticker Pack".to_owned() } else { pack.title.clone() };
    let (open_state, open_id, open_key) = (state.clone(), pack.id.clone(), pack.key.clone());
    let (menu_state, menu_pack) = (state.clone(), pack.clone());
    let control = install_button.then(|| {
        let (state, pack) = (state.clone(), pack.clone());
        Button::new(SharedString::from(format!("{id}-install")))
            .small()
            .outline()
            .when(pack.installed, |b| b.icon(IconName::Check).disabled(true).tooltip("Installed"))
            .when(!pack.installed, |b| b.icon(IconName::Plus).tooltip("Install"))
            .on_click(move |_, _, cx| state.read(cx).send(Command::InstallStickerPack { id: pack.id.clone(), key: pack.key.clone() }))
    });
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .py_1p5()
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(palette.row_hover))
        .on_click(move |_, _, cx| open_state.update(cx, |s, cx| s.open_sticker_pack(open_id.clone(), open_key.clone(), cx)))
        .child(cover(pack, 48., palette))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .child(div().truncate().child(title))
                        .when(pack.blessed, |d| d.child(crate::views::common::official_badge(palette))),
                )
                .child(div().text_xs().text_color(palette.text_secondary).truncate().child(pack.author.clone())),
        )
        .children(control)
        .test_support()
        .context_menu(move |menu, _, _| {
            let mut menu = menu;
            if let Some((up, down)) = &moves {
                for (label, order) in [("Move up", up), ("Move down", down)] {
                    if let Some(order) = order.clone() {
                        let state = menu_state.clone();
                        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| state.read(cx).send(Command::OrderStickerPacks(order.clone()))));
                    }
                }
            }
            let (state, pack) = (menu_state.clone(), menu_pack.clone());
            if pack.installed {
                menu.item(PopupMenuItem::new("Remove").on_click(move |_, window, cx| uninstall(state.clone(), pack.clone(), window, cx)))
            } else {
                menu.item(PopupMenuItem::new("Add").on_click(move |_, _, cx| {
                    state.read(cx).send(Command::InstallStickerPack { id: pack.id.clone(), key: pack.key.clone() })
                }))
            }
        })
        .into_any_element()
}

/// Opens Desktop's sticker manager.
pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    state.read(cx).send(Command::LoadStickerManager);
    let tab = cx.new(|_| Tab::default());
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let current = *tab.read(cx);
        let view = state.read(cx).sticker_manager.clone().unwrap_or_default();
        let tab_button = |id: &'static str, label: &'static str, which: Tab| {
            let tab = tab.clone();
            Button::new(id).label(label).small().map(|b| if current == which { b.primary() } else { b.ghost() }).on_click(move |_, window, cx| {
                tab.update(cx, |t, _| *t = which);
                window.refresh();
            })
        };
        let heading = |text: &'static str| div().mt_2().text_sm().font_weight(FontWeight::SEMIBOLD).child(text);
        let body = match current {
            Tab::All => {
                let blessed: Vec<AnyElement> = view
                    .blessed
                    .iter()
                    .enumerate()
                    .map(|(ix, p)| pack_row(format!("sticker-blessed-{ix}").into(), p, true, None, &state, palette))
                    .collect();
                let received: Vec<AnyElement> = view
                    .received
                    .iter()
                    .enumerate()
                    .map(|(ix, p)| pack_row(format!("sticker-received-{ix}").into(), p, true, None, &state, palette))
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(heading("Signal Artist Series"))
                    .when(blessed.is_empty(), |d| {
                        d.child(div().text_xs().text_color(palette.text_secondary).child("No Signal Artist stickers available"))
                    })
                    .children(blessed)
                    .when(!received.is_empty(), |d| {
                        d.child(heading("Shared With You"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.text_secondary)
                                    .child("When you receive a sticker from someone, the sticker pack will appear here."),
                            )
                            .children(received)
                    })
                    .into_any_element()
            }
            Tab::MyStickers if view.installed.is_empty() => {
                let tab = tab.clone();
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .py_8()
                    .child(div().text_sm().text_color(palette.text_secondary).child("No sticker packs, add stickers to send to your friends."))
                    .child(Button::new("sticker-add-stickers").label("Add stickers").outline().on_click(move |_, window, cx| {
                        tab.update(cx, |t, _| *t = Tab::All);
                        window.refresh();
                    }))
                    .into_any_element()
            }
            Tab::MyStickers => {
                let ids: Vec<Vec<u8>> = view.installed.iter().map(|p| p.id.clone()).collect();
                let moved = |from: usize, to: usize| {
                    let mut order = ids.clone();
                    let id = order.remove(from);
                    order.insert(to, id);
                    order
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(view.installed.iter().enumerate().map(|(ix, p)| {
                        let up = (ix > 0).then(|| moved(ix, ix - 1));
                        let down = (ix + 1 < ids.len()).then(|| moved(ix, ix + 1));
                        pack_row(format!("sticker-installed-{ix}").into(), p, false, Some((up, down)), &state, palette)
                    }))
                    .into_any_element()
            }
        };
        dialog.title("Stickers").w(px(480.)).child(
            div()
                .debug_selector(|| "sticker-manager".into())
                .flex()
                .flex_col()
                .gap_2()
                .child(div().flex().justify_center().gap_2().child(tab_button("sticker-tab-all", "All", Tab::All)).child(tab_button(
                    "sticker-tab-mine",
                    "My Stickers",
                    Tab::MyStickers,
                )))
                .child(div().id("sticker-manager-list").max_h(px(460.)).overflow_y_scroll().child(body)),
        )
    });
}

/// Desktop's `StickerPreviewModal` for the pack `AppState::open_sticker_pack`
/// opened: loading, the error, or the pack with Link, Remove and Add
/// Stickers.
pub fn open_preview(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let preview = state.read(cx).sticker_preview.clone();
        let dialog = dialog.title("Sticker Pack").w(px(440.));
        match preview {
            None | Some(StickerPreview::Loading(_)) => {
                dialog.child(div().debug_selector(|| "sticker-preview-loading".into()).flex().justify_center().py_6().child(Spinner::new().large()))
            }
            Some(StickerPreview::Failed(error)) => dialog.child(
                div().debug_selector(|| "sticker-preview-error".into()).px_6().py_6().text_center().text_color(palette.danger).child(error),
            ),
            Some(StickerPreview::Ready(preview)) => {
                let pack = preview.pack.clone();
                let count = if pack.sticker_count == 1 { "1 sticker".to_owned() } else { format!("{} stickers", pack.sticker_count) };
                let link = signal_backend::stickers::pack_link(&pack.id, &pack.key);
                let remove = pack.installed.then(|| {
                    let (state, pack) = (state.clone(), pack.clone());
                    Button::new("sticker-preview-remove").label("Remove").outline().small().icon(IconName::Minus).on_click(move |_, window, cx| {
                        uninstall(state.clone(), pack.clone(), window, cx)
                    })
                });
                let body = div()
                    .debug_selector(|| "sticker-preview".into())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(cover(&pack, 80., palette))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(pack.title.clone())
                            .when(pack.blessed, |d| d.child(crate::views::common::official_badge(palette))),
                    )
                    .child(div().text_sm().text_color(palette.text_secondary).child(pack.author.clone()))
                    .child(div().text_sm().text_color(palette.text_secondary).child(count))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("sticker-preview-link").label("Link").outline().small().icon(IconName::Link).on_click(
                                move |_, window, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(link.clone()));
                                    window.push_notification(Notification::new().message("Link copied"), cx);
                                },
                            ))
                            .children(remove),
                    )
                    .child(div().id("sticker-preview-grid").max_h(px(320.)).overflow_y_scroll().child(
                        div().flex().flex_wrap().gap_2().justify_center().children(preview.stickers.iter().map(|sticker| {
                            img(sticker.path.clone()).size(px(72.)).object_fit(gpui_kit::ObjectFit::Contain)
                        })),
                    ));
                let install = (!pack.installed).then(|| {
                    let (state, pack) = (state.clone(), pack.clone());
                    Button::new("sticker-preview-install").label("Add Stickers").primary().on_click(move |_, _, cx| {
                        state.read(cx).send(Command::InstallStickerPack { id: pack.id.clone(), key: pack.key.clone() })
                    })
                });
                dialog.child(body).footer(DialogFooter::new().children(install))
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::*;
    use crate::views::test_support::{demo, press, shows, wait};

    #[gpui_kit::test]
    fn the_manager_installs_a_shared_pack_and_its_preview_opens(cx: &mut TestAppContext) {
        let (_backend, state, mut cx, _dir) = demo(cx);
        cx.update(|window, cx| open(state.clone(), window, cx));
        wait(&mut cx, "the manager's packs", &mut |cx| cx.read(|cx| state.read(cx).sticker_manager.is_some()));
        assert!(shows(&mut cx, "sticker-manager"));
        // The demo's packs are shared with us; Garden dots isn't installed.
        let garden = cx.read(|cx| state.read(cx).sticker_manager.clone().unwrap().received[1].clone());
        assert_eq!((garden.title.as_str(), garden.installed), ("Garden dots", false));
        press(&mut cx, "sticker-received-1-install");
        wait(&mut cx, "the install", &mut |cx| cx.read(|cx| state.read(cx).sticker_packs.len() == 2));

        // My Stickers lists both, in order.
        press(&mut cx, "sticker-tab-mine");
        let installed = cx.read(|cx| state.read(cx).sticker_manager.clone().unwrap().installed);
        assert_eq!(installed.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(), ["Demo shapes", "Garden dots"]);

        // Opening a pack (as a signal.art link or a row does) shows it.
        let link = signal_backend::stickers::pack_link(&garden.id, &garden.key);
        cx.update(|window, cx| {
            assert!(state.update(cx, |s, cx| s.open_signal_link(&link, cx)));
            open_preview(state.clone(), window, cx);
        });
        wait(&mut cx, "the preview", &mut |cx| shows(cx, "sticker-preview"));
        let Some(StickerPreview::Ready(preview)) = cx.read(|cx| state.read(cx).sticker_preview.clone()) else { panic!() };
        assert_eq!((preview.stickers.len(), preview.pack.installed), (4, true));
    }
}
