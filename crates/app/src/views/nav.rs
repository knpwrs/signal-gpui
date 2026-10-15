//! Desktop's navigation tabs (`NavTabs`): a rail with "Hide Tabs" at the
//! top, then Chats, Calls and Stories, and Settings at the bottom. Hidden,
//! its toggle moves into the chat list's header with the other tabs'
//! badges (`NavTabsToggle`).

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, Hsla, SharedString, div, px, svg};

use crate::state::{AppState, NavTab};
use crate::theme::Palette;

/// Desktop's `$NavTabs__width`.
pub const NAV_WIDTH: f32 = 80.;

/// A tab's icon: Desktop's, filled when selected.
fn icon(name: &str, selected: bool, color: Hsla) -> impl IntoElement {
    let path = if selected { format!("icons/signal/nav-{name}-fill.svg") } else { format!("icons/signal/nav-{name}.svg") };
    svg().path(SharedString::from(path)).size(px(20.)).flex_none().text_color(color)
}

/// Desktop's unread badge: a red pill on the icon's corner.
pub fn badge(count: usize, palette: Palette) -> Option<AnyElement> {
    (count > 0).then(|| {
        div()
            .absolute()
            .top(px(-4.))
            .right(px(-6.))
            .min_w(px(16.))
            .h(px(16.))
            .px(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(palette.danger)
            .text_color(Hsla { h: 0., s: 0., l: 1., a: 1. })
            .text_size(px(10.))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .child(if count > 99 { "99+".to_owned() } else { count.to_string() })
            .into_any_element()
    })
}

/// One of the rail's buttons.
fn item(
    id: &'static str,
    label: &'static str,
    icon_name: &'static str,
    selected: bool,
    count: usize,
    palette: Palette,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    div().w_full().px(px(10.)).py(px(2.)).child(
        div()
            .id(id)
            .debug_selector(move || id.into())
            .w_full()
            .py(px(10.))
            .flex()
            .justify_center()
            .rounded(px(8.))
            .cursor_pointer()
            .when(selected, |d| d.bg(palette.row_selected))
            .when(!selected, |d| d.hover(|d| d.bg(palette.row_hover)))
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx))
            .on_click(move |_, _, cx| on_click(cx))
            .child(
                div()
                    .relative()
                    .child(icon(icon_name, selected, palette.text))
                    .children(badge(count, palette)),
            ),
    )
}

/// The rail.
pub fn render(state: &Entity<AppState>, palette: Palette, window: &gpui_kit::Window, cx: &mut App) -> AnyElement {
    let (tab, chats, stories, stories_on) = {
        let s = state.read(cx);
        (s.tab, s.unread_chats_badge() as usize, s.unread_story_chats(), !s.stories_disabled)
    };
    let tab_item = |id, label, icon_name, which: NavTab, count| {
        let state = state.clone();
        item(id, label, icon_name, tab == which, count, palette, move |cx| {
            state.update(cx, |state, cx| state.set_tab(which, cx));
        })
    };
    let toggle = {
        let state = state.clone();
        item("nav-toggle", "Hide Tabs", "menu", false, 0, palette, move |cx| state.update(cx, |s, cx| s.toggle_nav(cx)))
    };
    let settings = {
        let state = state.clone();
        div().w_full().px(px(10.)).py(px(2.)).child(
            div()
                .id("nav-settings")
                .w_full()
                .py(px(10.))
                .flex()
                .justify_center()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Settings").build(window, cx))
                .on_click(move |_, window, cx| crate::views::settings::open(state.clone(), window, cx))
                .child(icon("settings", false, palette.text)),
        )
    };
    div()
        .id("nav-tabs")
        .debug_selector(|| "nav-tabs".into())
        .w(px(NAV_WIDTH))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        // Under the macOS traffic lights.
        .pt(crate::views::common::title_bar_inset(window))
        .pb_2()
        .bg(palette.sidebar)
        .border_r_1()
        .border_color(palette.panel_border)
        .child(toggle)
        .child(tab_item("nav-chats", "Chats", "chat", NavTab::Chats, chats))
        .child(tab_item("nav-calls", "Calls", "phone", NavTab::Calls, 0))
        .when(stories_on, |d| d.child(tab_item("nav-stories", "Stories", "stories", NavTab::Stories, stories)))
        .child(div().flex_1())
        .child(settings)
        .into_any_element()
}

/// The toggle in the chat list's header while the tabs are hidden ("Show
/// Tabs"), with the other tabs' unread counts on it.
pub fn show_tabs_button(state: &Entity<AppState>, palette: Palette, cx: &App) -> AnyElement {
    let others = state.read(cx).unread_story_chats();
    let state = state.clone();
    div()
        .id("nav-show-tabs")
        .debug_selector(|| "nav-show-tabs".into())
        .flex_none()
        .p(px(6.))
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|d| d.bg(palette.row_hover))
        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Show Tabs").build(window, cx))
        .on_click(move |_, _, cx| state.update(cx, |s, cx| s.toggle_nav(cx)))
        .child(div().relative().child(icon("menu", false, palette.text)).children(badge(others, palette)))
        .into_any_element()
}
