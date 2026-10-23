//! Emoji picker: search, recents, category tabs and skin tones. From the
//! composer it also has Signal Desktop's Stickers and GIFs tabs.
//!
//! Searching uses the same FST index as composer type-ahead, boosted by the
//! user's own usage, so the picker and `:shortcode` completion agree.

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::{Icon, input::Enter};
use gpui_kit::assets::IconName;
use gpui_kit::prelude::*;
use std::sync::Arc;

use gpui_kit::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ObjectFit, SharedString, Subscription,
    UniformListScrollHandle, Window, div, img, px, uniform_list,
};
use signal_backend::{StickerPackView, StickerView};
use signal_emoji::{Emoji, Group, SearchOptions, SkinTone};

use crate::state::AppState;
use crate::theme::Palette;

pub const COLUMNS: usize = 9;
const CELL: f32 = 38.;
pub const PICKER_WIDTH: f32 = CELL * COLUMNS as f32 + 28.; // padding, border, scrollbar
const GRID_HEIGHT: f32 = CELL * 8.;
const SKIN_TONE_SETTING: &str = "emoji.skin_tone";

pub enum PickerEvent {
    Picked(String),
    Sticker(Arc<StickerPackView>, StickerView),
    Gif(signal_backend::giphy::Gif),
    Dismissed,
    /// Desktop's "Customize reactions" button, offered when picking a
    /// reaction.
    Customize,
}

/// The picker's panels (Signal Desktop's "Fun" picker).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Emoji,
    Stickers,
    Gifs,
}

/// A titled run of stickers in the picker.
type StickerSection = (SharedString, Vec<(Arc<StickerPackView>, StickerView)>);

/// Desktop's sticker grid: four across.
const STICKER_COLUMNS: usize = 4;
const STICKER_CELL: f32 = (PICKER_WIDTH - 28.) / STICKER_COLUMNS as f32;
/// Desktop's GIF grid: two columns, each GIF at its own height.
const GIF_COLUMN: f32 = (PICKER_WIDTH - 28. - 8.) / 2.;
const GIF_SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// The panel below the search box keeps one height across tabs.
const PANEL_HEIGHT: f32 = GRID_HEIGHT + 72.;

enum Row {
    Header(&'static str),
    Emoji(Vec<&'static Emoji>),
}

pub struct EmojiPicker {
    state: Entity<AppState>,
    search: Entity<InputState>,
    query: String,
    rows: Vec<Row>,
    group_rows: Vec<(Group, usize)>,
    /// Keyboard highlight within search results (flat index).
    highlighted: usize,
    hovered: Option<&'static Emoji>,
    skin_tone: Option<SkinTone>,
    choosing_tone: bool,
    tab: Tab,
    /// Stickers and GIFs are offered from the composer, not for reactions.
    media: bool,
    /// Offers "Customize reactions".
    customizable: bool,
    /// GIF results for `gif_query` (this page and earlier ones).
    gifs: Vec<(signal_backend::giphy::Gif, std::path::PathBuf)>,
    gif_query: Option<String>,
    gif_next: Option<u32>,
    gif_loading: bool,
    gif_error: Option<String>,
    gif_task: Option<gpui_kit::Task<()>>,
    giphy_key: Entity<InputState>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for EmojiPicker {}

pub fn tone_from_setting(value: Option<&str>) -> Option<SkinTone> {
    let ix: usize = value?.parse().ok()?;
    SkinTone::ALL.get(ix).copied()
}

fn tone_to_setting(tone: Option<SkinTone>) -> String {
    tone.and_then(|t| SkinTone::ALL.iter().position(|x| *x == t))
        .map_or_else(|| "none".into(), |ix| ix.to_string())
}

impl EmojiPicker {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search emoji"));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event: &InputEvent, _, cx| match event {
                InputEvent::Change => {
                    this.query = input.read(cx).value().to_string();
                    this.highlighted = 0;
                    this.rebuild(cx);
                    if this.tab == Tab::Gifs {
                        this.search_gifs(true, cx);
                    }
                }
                InputEvent::PressEnter { .. } => this.pick_highlighted(cx),
                _ => {}
            }),
            cx.subscribe(&state, |this, _, event: &crate::state::AppEvent, cx| {
                if let crate::state::AppEvent::Gifs { query, offset, result } = event {
                    // Only the answer to the latest search counts.
                    if this.gif_query.as_deref() != Some(query.as_str()) {
                        return;
                    }
                    this.gif_loading = false;
                    match result {
                        Ok(page) => {
                            if *offset == 0 {
                                this.gifs.clear();
                            }
                            this.gifs.extend(page.gifs.iter().cloned());
                            this.gif_next = page.next;
                            this.gif_error = None;
                        }
                        Err(error) => this.gif_error = Some(error.clone()),
                    }
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            state: state.clone(),
            search,
            query: String::new(),
            rows: Vec::new(),
            group_rows: Vec::new(),
            highlighted: 0,
            hovered: None,
            skin_tone: None,
            choosing_tone: false,
            tab: Tab::Emoji,
            media: false,
            customizable: false,
            gifs: Vec::new(),
            gif_query: None,
            gif_next: None,
            gif_loading: false,
            gif_error: None,
            gif_task: None,
            giphy_key: cx.new(|cx| InputState::new(window, cx).placeholder("Paste your Giphy API key")),
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        let picker = cx.entity().downgrade();
        state.update(cx, |state, cx| {
            state.setting(SKIN_TONE_SETTING, cx, move |_, value, cx| {
                let _ = picker.update(cx, |picker, cx| {
                    picker.skin_tone = tone_from_setting(value.as_deref());
                    cx.notify();
                });
            })
        });
        this
    }

    /// Whether the Stickers and GIFs tabs are offered (from the composer).
    pub fn set_media(&mut self, media: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.media = media;
        self.customizable = false;
        if !media && self.tab != Tab::Emoji {
            self.set_tab(Tab::Emoji, window, cx);
        }
        if media {
            // Packs installed on the phone since last time.
            self.state.read(cx).send(signal_backend::Command::LoadStickers);
        }
        cx.notify();
    }

    /// Offers "Customize reactions" (picking a reaction; Desktop's
    /// `showCustomizePreferredReactionsButton`).
    pub fn set_customizable(&mut self, customizable: bool, cx: &mut Context<Self>) {
        self.customizable = customizable;
        cx.notify();
    }

    /// Searches (trending when empty), after a pause while typing.
    fn search_gifs(&mut self, debounce: bool, cx: &mut Context<Self>) {
        if !self.state.read(cx).has_giphy_key {
            return;
        }
        let query = self.query.trim().to_owned();
        self.gif_query = Some(query.clone());
        self.gif_loading = true;
        self.gif_error = None;
        self.gif_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(GIF_SEARCH_DEBOUNCE).await;
            }
            let _ = this.update(cx, |this, cx| {
                this.state.read(cx).send(signal_backend::Command::SearchGifs { query, offset: 0 });
            });
        }));
        cx.notify();
    }

    fn more_gifs(&mut self, cx: &mut Context<Self>) {
        // Another request for the same page would add it twice.
        if self.gif_loading {
            return;
        }
        if let (Some(query), Some(offset)) = (self.gif_query.clone(), self.gif_next) {
            self.gif_loading = true;
            self.state.read(cx).send(signal_backend::Command::SearchGifs { query, offset });
            cx.notify();
        }
    }

    fn render_gifs(&self, palette: Palette, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        if !self.state.read(cx).has_giphy_key {
            return self.render_giphy_setup(palette, cx).into_any_element();
        }
        // Two columns, each GIF placed in the shorter one (Desktop's grid).
        let mut columns: [Vec<gpui_kit::AnyElement>; 2] = [Vec::new(), Vec::new()];
        let mut heights = [0f32; 2];
        for (ix, (gif, preview)) in self.gifs.iter().enumerate() {
            let height = GIF_COLUMN * gif.preview.height as f32 / gif.preview.width.max(1) as f32;
            let frames = self.state.update(cx, |state, cx| state.video_loop(preview, (GIF_COLUMN * 2.) as u32, cx));
            let column = if heights[0] <= heights[1] { 0 } else { 1 };
            heights[column] += height + 4.;
            let pick = gif.clone();
            columns[column].push(
                div()
                    .id(SharedString::from(format!("gif-{ix}")))
                    .w(px(GIF_COLUMN))
                    .h(px(height))
                    .rounded_md()
                    .overflow_hidden()
                    .bg(palette.row_hover)
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PickerEvent::Gif(pick.clone()))))
                    // With an id, so gpui steps its frames.
                    .when_some(frames, |d, frames| {
                        d.child(img(frames).id(SharedString::from(format!("gif-frames-{ix}"))).size_full().object_fit(ObjectFit::Cover))
                    })
                    .into_any_element(),
            );
        }
        let status = if let Some(error) = &self.gif_error {
            Some(error.clone())
        } else if self.gifs.is_empty() && !self.gif_loading && self.gif_query.is_some() {
            Some("No GIFs found".into())
        } else {
            None
        };
        div()
            .h(px(PANEL_HEIGHT))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("gif-grid")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .when_some(status, |d, status| {
                        d.child(div().p_4().text_sm().text_color(palette.text_secondary).child(status))
                    })
                    .child(div().flex().gap_2().children(
                        columns.into_iter().map(|items| div().flex().flex_col().gap_1().children(items)),
                    ))
                    .when(self.gif_next.is_some() && !self.gifs.is_empty(), |d| {
                        d.child(
                            div().py_2().flex().justify_center().child(
                                gpui_kit::component::button::Button::new("more-gifs")
                                    .label(if self.gif_loading { "Loading…" } else { "Load more" })
                                    .on_click(cx.listener(|this, _, _, cx| this.more_gifs(cx))),
                            ),
                        )
                    }),
            )
            // Giphy asks apps to credit it.
            .child(
                div()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(palette.text_secondary)
                    .child("Powered by GIPHY"),
            )
            .into_any_element()
    }

    fn render_giphy_setup(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let key = self.giphy_key.clone();
        div()
            .h(px(PANEL_HEIGHT))
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .text_sm()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("GIF search needs a Giphy API key"))
            .child(div().text_color(palette.text_secondary).child(
                "Signal's own key is for Signal's apps, so this client uses yours. Create a free one at \
                 developers.giphy.com (an \"API\" app), then paste it here. Searches go through Signal's \
                 content proxy, so Giphy doesn't see your IP address.",
            ))
            .child(Input::new(&self.giphy_key))
            .child(
                div().flex().gap_2().child(
                    gpui_kit::component::button::Button::new("save-giphy-key")
                        .label("Save key")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let value = key.read(cx).value().to_string();
                            if value.trim().is_empty() {
                                return;
                            }
                            this.state.update(cx, |state, cx| state.set_giphy_key(value, cx));
                            this.search_gifs(false, cx);
                        })),
                ),
            )
            .child(
                div()
                    .id("giphy-link")
                    .text_color(palette.accent)
                    .cursor_pointer()
                    .on_click(|_, _, cx| cx.open_url("https://developers.giphy.com/dashboard/"))
                    .child("Open developers.giphy.com"),
            )
    }

    /// The panel showing.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// Shows a panel (from a shortcut); Stickers and GIFs only where
    /// they're offered.
    pub fn show_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        if tab != self.tab && (self.media || tab == Tab::Emoji) {
            self.set_tab(tab, window, cx);
        }
    }

    fn set_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        let placeholder = match tab {
            Tab::Emoji => "Search emoji",
            Tab::Stickers => "Search stickers",
            Tab::Gifs => "Search GIFs",
        };
        self.search.update(cx, |s, cx| {
            s.set_placeholder(placeholder, window, cx);
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        self.query.clear();
        self.rebuild(cx);
        if tab == Tab::Gifs {
            // Trending, until you type.
            self.search_gifs(false, cx);
        }
    }

    /// Stickers to show: recents and every pack, or those whose emoji
    /// matches the search (Desktop searches emoji, then their stickers).
    fn sticker_sections(&self, cx: &App) -> Vec<StickerSection> {
        let state = self.state.read(cx);
        let packs = &state.sticker_packs;
        let query = self.query.trim();
        if !query.is_empty() {
            let options = SearchOptions { limit: 60, max_version: Self::max_version(), fuzzy: true };
            let mut wanted: std::collections::HashSet<&'static str> =
                signal_emoji::search(query, &options).into_iter().map(|m| m.emoji.glyph).collect();
            if let Some((emoji, _)) = signal_emoji::by_glyph(query) {
                wanted.insert(emoji.glyph);
            }
            let matches: Vec<_> = packs
                .iter()
                .flat_map(|pack| pack.stickers.iter().map(move |s| (pack.clone(), s.clone())))
                .filter(|(_, sticker)| {
                    sticker
                        .emoji
                        .as_deref()
                        .and_then(signal_emoji::by_glyph)
                        .is_some_and(|(emoji, _)| wanted.contains(emoji.glyph))
                })
                .collect();
            return if matches.is_empty() { Vec::new() } else { vec![("Results".into(), matches)] };
        }
        let mut sections = Vec::new();
        let recents: Vec<_> = state
            .recent_stickers
            .iter()
            .filter_map(|(pack_id, id)| {
                let pack = packs.iter().find(|p| p.id == *pack_id)?;
                let sticker = pack.stickers.iter().find(|s| s.id == *id)?;
                Some((pack.clone(), sticker.clone()))
            })
            .collect();
        if !recents.is_empty() {
            sections.push(("Recently used".into(), recents));
        }
        for pack in packs {
            let stickers = pack.stickers.iter().map(|s| (pack.clone(), s.clone())).collect();
            sections.push((pack.title.clone().into(), stickers));
        }
        sections
    }

    fn render_stickers(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = self.sticker_sections(cx);
        let empty = if !self.query.trim().is_empty() {
            "No stickers found"
        } else {
            "No sticker packs installed. Add some with + or in Signal on your phone."
        };
        let manage_state = self.state.clone();
        div()
            .id("sticker-grid")
            .h(px(PANEL_HEIGHT))
            .overflow_y_scroll()
            .px_2()
            .pb_2()
            // Desktop's "+" in the sticker picker opens the sticker manager.
            .child(
                div().flex().justify_end().pt_1().child(
                    gpui_kit::component::button::Button::new("sticker-manager-open")
                        .map(gpui_kit::component::button::ButtonVariants::ghost)
                        .map(gpui_kit::component::Sizable::small)
                        .icon(gpui_kit::assets::IconName::Plus)
                        .tooltip("Add stickers")
                        .on_click(move |_, window, cx| crate::views::sticker_manager::open(manage_state.clone(), window, cx)),
                ),
            )
            .when(sections.is_empty(), |d| {
                d.child(div().p_4().text_sm().text_color(palette.text_secondary).child(empty))
            })
            .children(sections.into_iter().enumerate().map(|(section_ix, (title, stickers))| {
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(28.))
                            .px_1()
                            .flex()
                            .items_end()
                            .pb_1()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.text_secondary)
                            .child(title),
                    )
                    .child(div().flex().flex_wrap().children(stickers.into_iter().enumerate().map(
                        |(ix, (pack, sticker))| {
                            let path = sticker.path.clone();
                            div()
                                .id(SharedString::from(format!("sticker-{section_ix}-{ix}")))
                                .size(px(STICKER_CELL))
                                .p_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|d| d.bg(palette.row_hover))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(PickerEvent::Sticker(pack.clone(), sticker.clone()))
                                }))
                                .child(img(path).size_full().object_fit(ObjectFit::Contain))
                        },
                    )))
            }))
    }

    fn render_tab_bar(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: &'static str, which: Tab| {
            let selected = self.tab == which;
            div()
                .id(id)
                .flex_1()
                .h(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .when(selected, |d| d.bg(palette.row_selected))
                .when(!selected, |d| d.text_color(palette.text_secondary).hover(|d| d.bg(palette.row_hover)))
                .on_click(cx.listener(move |this, _, window, cx| this.set_tab(which, window, cx)))
                .child(label)
        };
        div()
            .flex()
            .gap_1()
            .px_2()
            .pt_2()
            .child(tab("tab-emoji", "Emoji", Tab::Emoji))
            .child(tab("tab-stickers", "Stickers", Tab::Stickers))
            .child(tab("tab-gifs", "GIFs", Tab::Gifs))
    }

    pub fn skin_tone(&self) -> Option<SkinTone> {
        self.skin_tone
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.query.clear();
        self.highlighted = 0;
        self.choosing_tone = false;
        self.rebuild(cx);
        self.scroll.scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    fn max_version() -> f32 {
        crate::platform::max_emoji_version()
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let max_version = Self::max_version();
        self.rows.clear();
        self.group_rows.clear();
        let push_grid = |rows: &mut Vec<Row>, emoji: Vec<&'static Emoji>| {
            for chunk in emoji.chunks(COLUMNS) {
                rows.push(Row::Emoji(chunk.to_vec()));
            }
        };

        let query = self.query.trim();
        if query.is_empty() {
            let recents: Vec<&'static Emoji> = self
                .state
                .read(cx)
                .emoji_usage
                .recents()
                .iter()
                .filter_map(|g| signal_emoji::by_glyph(g).map(|(e, _)| e))
                .take(COLUMNS * 2)
                .collect();
            if !recents.is_empty() {
                self.rows.push(Row::Header("Recently used"));
                push_grid(&mut self.rows, recents);
            }
            for group in Group::PICKER {
                self.group_rows.push((group, self.rows.len()));
                self.rows.push(Row::Header(group.label()));
                push_grid(
                    &mut self.rows,
                    signal_emoji::in_group(group).filter(|e| e.version <= max_version).collect(),
                );
            }
        } else {
            let usage = &self.state.read(cx).emoji_usage;
            let options = SearchOptions {
                limit: COLUMNS * 12,
                max_version,
                fuzzy: true,
            };
            let results: Vec<&'static Emoji> = signal_emoji::search_with_boost(query, &options, |id| usage.boost(id))
                .into_iter()
                .map(|m| m.emoji)
                .collect();
            push_grid(&mut self.rows, results);
        }
        cx.notify();
    }

    fn search_results(&self) -> impl Iterator<Item = &'static Emoji> + '_ {
        self.rows.iter().flat_map(|row| match row {
            Row::Emoji(emoji) => emoji.clone(),
            Row::Header(_) => Vec::new(),
        })
    }

    fn pick(&mut self, emoji: &'static Emoji, cx: &mut Context<Self>) {
        let glyph = emoji.glyph_with(self.skin_tone).to_owned();
        cx.emit(PickerEvent::Picked(glyph));
    }

    fn pick_highlighted(&mut self, cx: &mut Context<Self>) {
        if self.query.trim().is_empty() {
            return;
        }
        let emoji = self.search_results().nth(self.highlighted);
        if let Some(emoji) = emoji {
            self.pick(emoji, cx);
        }
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.search_results().count();
        if count == 0 {
            return;
        }
        self.highlighted = (self.highlighted as isize + delta).clamp(0, count as isize - 1) as usize;
        self.scroll.scroll_to_item(self.highlighted / COLUMNS, gpui_kit::ScrollStrategy::Center);
        cx.notify();
    }

    fn set_skin_tone(&mut self, tone: Option<SkinTone>, cx: &mut Context<Self>) {
        self.skin_tone = tone;
        self.choosing_tone = false;
        self.state.read(cx).set_setting(SKIN_TONE_SETTING, tone_to_setting(tone));
        cx.notify();
    }

    fn render_cell(&self, emoji: &'static Emoji, flat_index: Option<usize>, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let highlighted = flat_index.is_some_and(|ix| ix == self.highlighted && !self.query.trim().is_empty());
        div()
            .id(SharedString::from(format!("emoji-{}", emoji.id)))
            .size(px(CELL))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(26.))
            .cursor_pointer()
            .when(highlighted, |d| d.bg(palette.row_selected))
            .hover(|d| d.bg(palette.row_hover))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hovered = Some(emoji);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.pick(emoji, cx)))
            .child(emoji.glyph_with(self.skin_tone))
    }

    fn render_footer(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tone_button = {
            let sample = signal_emoji::by_shortcode("wave").map_or("👋", |e| e.glyph_with(self.skin_tone));
            div()
                .id("skin-tone")
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_size(px(20.))
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.choosing_tone = !this.choosing_tone;
                    cx.notify();
                }))
                .child(sample)
        };

        if self.choosing_tone {
            let options = std::iter::once(None).chain(SkinTone::ALL.into_iter().map(Some));
            return div()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .h(px(40.))
                .children(options.enumerate().map(|(ix, tone)| {
                    let glyph = signal_emoji::by_shortcode("wave").map_or("👋", |e| e.glyph_with(tone));
                    div()
                        .id(("tone", ix))
                        .size(px(32.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .text_size(px(22.))
                        .cursor_pointer()
                        .when(tone == self.skin_tone, |d| d.bg(palette.row_selected))
                        .hover(|d| d.bg(palette.row_hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_skin_tone(tone, cx)))
                        .child(glyph)
                }))
                .into_any_element();
        }

        let info = match self.hovered {
            Some(emoji) => div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(div().text_size(px(22.)).child(emoji.glyph_with(self.skin_tone)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(emoji.name))
                        .when_some(emoji.shortcode(), |d, code| {
                            d.child(div().text_xs().text_color(palette.text_secondary).child(format!(":{code}:")))
                        }),
                ),
            None => div().text_sm().text_color(palette.text_secondary).child("Pick an emoji"),
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_2()
            .h(px(40.))
            .child(info)
            .child(tone_button)
            .into_any_element()
    }

    fn render_tabs(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .justify_between()
            .px_1()
            .border_t_1()
            .border_color(palette.panel_border)
            .children(self.group_rows.iter().map(|&(group, row)| {
                div()
                    .id(SharedString::from(group.label()))
                    .size(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_size(px(18.))
                    .cursor_pointer()
                    .hover(|d| d.bg(palette.row_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll.scroll_to_item(row, gpui_kit::ScrollStrategy::Top);
                        cx.notify();
                    }))
                    .child(group.icon())
            }))
    }
}

impl Focusable for EmojiPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }
}

impl Render for EmojiPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let row_count = self.rows.len();
        let searching = !self.query.trim().is_empty();

        // Flat index of the first emoji in each row, for keyboard highlight.
        let mut row_starts = Vec::with_capacity(row_count);
        let mut flat = 0;
        for row in &self.rows {
            row_starts.push(flat);
            if let Row::Emoji(e) = row {
                flat += e.len();
            }
        }

        div()
            .key_context("EmojiPicker")
            .w(px(PICKER_WIDTH))
            .flex()
            .flex_col()
            .rounded_xl()
            .border_1()
            .border_color(palette.panel_border)
            .bg(palette.popover)
            .text_color(palette.text)
            .shadow_lg()
            .occlude()
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(PickerEvent::Dismissed);
                let _ = this;
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                cx.stop_propagation();
                this.move_highlight(-(COLUMNS as isize), cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                cx.stop_propagation();
                this.move_highlight(COLUMNS as isize, cx);
            }))
            .capture_action(cx.listener(|this, enter: &Enter, _, cx| {
                if !enter.shift {
                    cx.stop_propagation();
                    this.pick_highlighted(cx);
                }
            }))
            .when(self.media, |d| d.child(self.render_tab_bar(palette, cx)))
            .child(
                div()
                    .p_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.search)
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).size_4().text_color(palette.text_secondary)),
                        ),
                    )
                    .when(self.customizable, |d| {
                        d.child(
                            div()
                                .id("customize-reactions")
                                .debug_selector(|| "customize-reactions".into())
                                .size(px(32.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|d| d.bg(palette.row_hover))
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Customize reactions").build(window, cx)
                                })
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(PickerEvent::Customize)))
                                .child(Icon::new(IconName::Pencil).size_4().text_color(palette.text_secondary)),
                        )
                    }),
            )
            .when(self.tab == Tab::Stickers, |d| d.child(self.render_stickers(palette, cx)))
            .when(self.tab == Tab::Gifs, |d| d.child(self.render_gifs(palette, cx)))
            .when(self.tab == Tab::Emoji, |d| d.child(
                uniform_list(
                    "emoji-grid",
                    row_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let palette = Palette::get(cx);
                        range
                            .map(|ix| match &this.rows[ix] {
                                Row::Header(label) => div()
                                    .h(px(CELL))
                                    .px_2()
                                    .flex()
                                    .items_end()
                                    .pb_1()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(palette.text_secondary)
                                    .child(*label)
                                    .into_any_element(),
                                Row::Emoji(emoji) => {
                                    let start = row_starts[ix];
                                    let cells: Vec<_> = emoji
                                        .clone()
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, e)| this.render_cell(e, Some(start + i), palette, cx).into_any_element())
                                        .collect();
                                    div().h(px(CELL)).px_2().flex().children(cells).into_any_element()
                                }
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .h(px(GRID_HEIGHT)),
            ))
            .when(self.tab == Tab::Emoji && searching && row_count == 0, |d| {
                d.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(palette.text_secondary)
                        .child("No emoji found"),
                )
            })
            .when(self.tab == Tab::Emoji, |d| d.child(self.render_footer(palette, cx)))
            .when(self.tab == Tab::Emoji && !searching, |d| d.child(self.render_tabs(palette, cx)))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{TestAppContext, WindowOptions};
    use signal_backend::{Backend, Config, Event};

    use super::*;

    #[gpui_kit::test]
    fn sticker_search_matches_emoji_and_recents_come_first(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (backend, _events) = Backend::start(Config {
            data_dir: dir.path().to_owned(),
            db_key: [7; 32],
            device_name: "test".into(),
            demo: true,
            former_data_dir: None,
        })
        .unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let sticker = |id: u32, emoji: &str| StickerView {
            id,
            emoji: Some(emoji.into()),
            path: dir.path().join(format!("{id}.png")),
        };
        let pack = StickerPackView {
            id: vec![1],
            key: vec![2],
            title: "Pack".into(),
            author: "me".into(),
            stickers: vec![sticker(0, "🔥"), sticker(1, "😀"), sticker(2, "☕")],
        };
        state.update(cx, |state, cx| state.handle(Event::Stickers(vec![pack]), cx));
        let (window, picker) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| EmojiPicker::new(state.clone(), window, cx))
            })
            .unwrap()
        });
        let ids = |picker: &EmojiPicker, cx: &App| {
            picker
                .sticker_sections(cx)
                .into_iter()
                .map(|(title, stickers)| (title.to_string(), stickers.iter().map(|(_, s)| s.id).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
        };
        // Searching words finds stickers by their emoji.
        cx.update_window(window, |_, _, cx| {
            picker.update(cx, |p, _| p.query = "fire".into());
        })
        .unwrap();
        assert_eq!(cx.read(|cx| ids(picker.read(cx), cx)), vec![("Results".to_owned(), vec![0])]);

        // Sending one puts it in "Recently used" above the packs.
        let conversation = signal_store::ConversationId("d:x".into());
        state.update(cx, |state, cx| {
            let pack = state.sticker_packs[0].clone();
            let sticker = pack.stickers[2].clone();
            state.send_sticker(conversation, pack, sticker, None, cx);
        });
        cx.update_window(window, |_, _, cx| picker.update(cx, |p, _| p.query.clear())).unwrap();
        assert_eq!(
            cx.read(|cx| ids(picker.read(cx), cx)),
            vec![("Recently used".to_owned(), vec![2]), ("Pack".to_owned(), vec![0, 1, 2])]
        );
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn gif_results_fill_the_grid_and_previews_decode(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let preview = dir.path().join("preview.mp4");
        let made = std::process::Command::new("/opt/homebrew/bin/ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=200x112:rate=10:duration=1", "-pix_fmt", "yuv420p"])
            .arg(&preview)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available; skipping");
            return;
        }
        let (backend, _events) = Backend::start(Config {
            data_dir: dir.path().to_owned(),
            db_key: [7; 32],
            device_name: "test".into(),
            demo: true,
            former_data_dir: None,
        })
        .unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (_window, picker) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| EmojiPicker::new(state.clone(), window, cx))
            })
            .unwrap()
        });
        // A search is in flight for "cats"; its answer arrives.
        picker.update(cx, |p, _| p.gif_query = Some("cats".into()));
        let media = |url: &str| signal_backend::giphy::GifMedia { url: url.into(), width: 200, height: 112 };
        let gif = signal_backend::giphy::Gif {
            id: "a".into(),
            title: "cat".into(),
            preview: media("https://media0.giphy.com/a/200w.mp4"),
            original: media("https://media0.giphy.com/a/giphy.mp4"),
        };
        let results = signal_backend::GifResults { gifs: vec![(gif, preview.clone())], next: Some(30) };
        state.update(cx, |state, cx| {
            state.handle(Event::Gifs { query: "dogs".into(), offset: 0, result: Ok(Default::default()) }, cx);
            state.handle(Event::Gifs { query: "cats".into(), offset: 0, result: Ok(results) }, cx);
        });
        // The stale "dogs" answer was ignored.
        assert_eq!(picker.read_with(cx, |p, _| (p.gifs.len(), p.gif_next)), (1, Some(30)));

        // Its preview decodes into frames for the grid.
        let mut frames = None;
        for _ in 0..300 {
            frames = state.update(cx, |state, cx| state.video_loop(&preview, 200, cx));
            if frames.is_some() {
                break;
            }
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(frames.expect("decoded").frame_count(), 10);
        backend.shutdown();
    }
}
