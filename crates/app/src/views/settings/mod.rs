//! Settings (⌘,), laid out as Signal Desktop's Preferences
//! (`ts/components/Preferences.dom.tsx`): a list of pages on the left and
//! the chosen page's lists of rows, with Desktop's wording.

mod appearance;
mod calls;
mod chats;
mod data_usage;
mod general;
mod notifications;
mod privacy;
#[cfg(test)]
mod tests;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, SharedString, TestSupportExt as _, Window, div, px};

use crate::state::AppState;
use crate::theme::Palette;

pub use privacy::confirm_stories_off;

/// Desktop's pages (`SettingsPage`); the last ones open from another page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    General,
    Appearance,
    Chats,
    Calls,
    Notifications,
    Privacy,
    DataUsage,
}

impl Page {
    /// The pages in the list, in Desktop's order.
    const LIST: [Page; 7] =
        [Page::General, Page::Appearance, Page::Chats, Page::Calls, Page::Notifications, Page::Privacy, Page::DataUsage];

    /// Desktop's names (`Preferences__button--…`).
    fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Appearance => "Appearance",
            Page::Chats => "Chats",
            Page::Calls => "Calls",
            Page::Notifications => "Notifications",
            Page::Privacy => "Privacy",
            Page::DataUsage => "Data usage",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Page::General => "settings-page-general",
            Page::Appearance => "settings-page-appearance",
            Page::Chats => "settings-page-chats",
            Page::Calls => "settings-page-calls",
            Page::Notifications => "settings-page-notifications",
            Page::Privacy => "settings-page-privacy",
            Page::DataUsage => "settings-page-data-usage",
        }
    }

    /// The page this one opens from, for its back button.
    fn parent(self) -> Option<Page> {
        None
    }

    /// Which page in the list shows as chosen.
    fn in_list(self) -> Page {
        self.parent().unwrap_or(self)
    }
}

/// The open Settings: which page shows.
pub struct Settings {
    pub(crate) state: Entity<AppState>,
    pub(crate) page: Page,
}

pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    show(state, Page::General, window, cx);
}

/// Opens Settings at `page`.
pub(crate) fn show(state: Entity<AppState>, page: Page, window: &mut Window, cx: &mut App) -> Option<Entity<Settings>> {
    if window.has_active_dialog(cx) {
        return None;
    }
    state.read(cx).send(signal_backend::Command::LoadCallDevices);
    state.update(cx, |s, cx| s.refresh_open_at_login(cx));
    let settings = cx.new(|_| Settings { state, page });
    let shown = settings.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        dialog.title("Settings").w(px(780.)).child(render(&settings, window, cx))
    });
    Some(shown)
}

/// Shows another page.
pub(crate) fn go(settings: &Entity<Settings>, page: Page, window: &mut Window, cx: &mut App) {
    settings.update(cx, |s, _| s.page = page);
    window.refresh();
}

fn render(settings: &Entity<Settings>, window: &mut Window, cx: &mut App) -> AnyElement {
    let palette = Palette::get(cx);
    let page = settings.read(cx).page;
    let nav = div().flex().flex_col().gap_0p5().w(px(170.)).flex_none().child(profile_row(settings, palette, cx)).children(Page::LIST.map(|item| {
        let chosen = item == page.in_list();
        let settings = settings.clone();
        div()
            .id(item.id())
            .px_3()
            .py_1p5()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(chosen, |d| d.bg(palette.row_selected).font_weight(FontWeight::SEMIBOLD))
            .when(!chosen, |d| d.hover(|d| d.bg(palette.row_hover)))
            .child(item.title())
            .on_click(move |_, window, cx| go(&settings, item, window, cx))
            .test_support()
    }));
    let body = match page {
        Page::General => general::render(settings, palette, window, cx),
        Page::Appearance => appearance::render(settings, palette, cx),
        Page::Chats => chats::render(settings, palette, cx),
        Page::Calls => calls::render(settings, palette, cx),
        Page::Notifications => notifications::render(settings, palette, cx),
        Page::Privacy => privacy::render(settings, palette, cx),
        Page::DataUsage => data_usage::render(settings, palette, cx),
    };
    let header = div()
        .flex()
        .items_center()
        .gap_2()
        .when_some(page.parent(), |d, parent| {
            let settings = settings.clone();
            d.child(
                Button::new("settings-back")
                    .ghost()
                    .small()
                    .icon(gpui_kit::assets::IconName::ArrowLeft)
                    .tooltip("Go back")
                    .on_click(move |_, window, cx| go(&settings, parent, window, cx)),
            )
        })
        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title_of(settings, cx)));
    div()
        .flex()
        .gap_4()
        .h(px(560.))
        .child(nav)
        .child(
            div()
                .id("settings-content")
                .flex_1()
                .min_w_0()
                .overflow_y_scroll()
                .pr_2()
                .flex()
                .flex_col()
                .gap_5()
                .child(header)
                .child(body)
                .test_support(),
        )
        .into_any_element()
}

/// Desktop's profile row at the top of the list (`PreferencesProfile`):
/// our photo, name and username, opening the profile editor.
fn profile_row(settings: &Entity<Settings>, palette: Palette, cx: &App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let s = state.read(cx);
    let profile = s.own_profile.clone().unwrap_or_default();
    let aci = s.our_aci().unwrap_or_default().to_owned();
    let name = profile.name().unwrap_or_else(|| "Set up your profile".into());
    div()
        .id("edit-profile")
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .mb_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(palette.row_hover))
        .child(crate::views::common::person_avatar(profile.photo.as_deref(), &name, crate::theme::color_for(&aci), px(36.)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().debug_selector(|| "settings-profile-name".into()).text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(name))
                .when_some(profile.username.clone(), |d, username| {
                    d.child(div().text_xs().text_color(palette.text_secondary).truncate().child(username))
                }),
        )
        .on_click(move |_, window, cx| {
            window.close_all_dialogs(cx);
            crate::views::profile_editor::open(state.clone(), window, cx);
        })
        .test_support()
        .into_any_element()
}

fn title_of(settings: &Entity<Settings>, cx: &App) -> SharedString {
    settings.read(cx).page.title().into()
}

/// One of Desktop's lists (`AxoList`): an optional label above, rows in a
/// rounded box, and an optional explanation below.
pub(crate) fn list(
    label: Option<&'static str>,
    footer: Option<SharedString>,
    rows: impl IntoIterator<Item = AnyElement>,
    palette: Palette,
) -> AnyElement {
    let mut rows = rows.into_iter().peekable();
    let mut boxed = div().flex().flex_col().rounded_lg().border_1().border_color(palette.panel_border);
    while let Some(row) = rows.next() {
        let last = rows.peek().is_none();
        boxed = boxed.child(div().px_3().py_2p5().when(!last, |d| d.border_b_1().border_color(palette.panel_border)).child(row));
    }
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .when_some(label, |d, label| {
            d.child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child(label))
        })
        .child(boxed)
        .when_some(footer, |d, footer| d.child(div().text_xs().text_color(palette.text_secondary).child(footer)))
        .into_any_element()
}

/// A row's label and description.
fn label_block(label: impl Into<SharedString>, description: Option<SharedString>, palette: Palette) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(div().text_sm().child(label.into()))
        .when_some(description, |d, description| d.child(div().text_xs().text_color(palette.text_secondary).child(description)))
}

/// Desktop's switch row (`AxoSwitchItem`).
pub(crate) fn switch(
    id: &'static str,
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    checked: bool,
    disabled: bool,
    palette: Palette,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(label_block(label, description, palette))
        .child(Switch::new(id).checked(checked).disabled(disabled).on_click(move |on, window, cx| {
            on_change(*on, window, cx);
            window.refresh();
        }))
        .into_any_element()
}

/// Desktop's row with a button (`ItemWithAction`).
pub(crate) fn action(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    button: Button,
    palette: Palette,
) -> AnyElement {
    div().flex().items_center().gap_3().child(label_block(label, description, palette)).child(button.small()).into_any_element()
}

/// One choice in a [`select`].
pub(crate) struct Choice {
    pub label: SharedString,
    pub chosen: bool,
    pub on_click: OnChoose,
}

/// What choosing a [`Choice`] does.
pub(crate) type OnChoose = Box<dyn Fn(&mut Window, &mut App)>;

impl Choice {
    pub fn new(label: impl Into<SharedString>, chosen: bool, on_click: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self { label: label.into(), chosen, on_click: Box::new(on_click) }
    }
}

/// Desktop's select row (`AxoSelectItem`): the chosen value on a button
/// that drops down the choices.
pub(crate) fn select(
    id: &'static str,
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    choices: Vec<Choice>,
    disabled: bool,
    palette: Palette,
) -> AnyElement {
    let current = choices.iter().find(|c| c.chosen).map(|c| c.label.clone()).unwrap_or_default();
    let choices = std::rc::Rc::new(choices);
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(label_block(label, description, palette))
        .child(Button::new(id).outline().small().label(current).disabled(disabled).dropdown_menu(move |mut menu, _, _| {
            for (ix, choice) in choices.iter().enumerate() {
                let choices = choices.clone();
                menu = menu.item(PopupMenuItem::new(choice.label.clone()).checked(choice.chosen).on_click(move |_, window, cx| {
                    (choices[ix].on_click)(window, cx);
                    window.refresh();
                }));
            }
            menu
        }))
        .into_any_element()
}

/// A label and a value (`AxoTextItem`).
pub(crate) fn text(label: &'static str, value: impl Into<SharedString>, palette: Palette) -> AnyElement {
    div()
        .flex()
        .justify_between()
        .gap_4()
        .text_sm()
        .child(div().flex_none().child(label))
        .child(div().min_w_0().truncate().text_color(palette.text_secondary).child(value.into()))
        .into_any_element()
}
