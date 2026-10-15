//! Desktop's Stories tab (`StoriesTab`, `StoriesPane`, `MyStoryButton`,
//! `StoryListItem`): your stories at the top, then one row per person or
//! group with stories up (unviewed first), then the hidden ones.

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, Hsla, IntoElement, ObjectFit, Pixels, Render, SharedString, Window, div,
    img, linear_color_stop, linear_gradient, px,
};
use signal_store::{Story, StoryBackground, StoryContent, StoryFont, TextStory};

use crate::state::{AppState, StoryGroup};
use crate::theme::Palette;
use crate::views::common::{conversation_avatar, short_timestamp};

/// Desktop's story canvas (`TextAttachment`): drawn at 720 × 1280, scaled.
pub const CANVAS_WIDTH: f32 = 720.;
pub const CANVAS_HEIGHT: f32 = 1280.;
/// Desktop's list thumbnail.
const THUMB_WIDTH: f32 = 46.;
const THUMB_HEIGHT: f32 = 72.;

/// An ARGB color (Signal's).
pub fn argb(color: u32) -> Hsla {
    // ARGB to RGBA.
    let rgba = color.rotate_left(8);
    gpui_kit::rgba(rgba).into()
}

/// A text story's background: its color, or its gradient (a vertical one
/// with more than two colors is drawn as bands between them).
pub fn story_background(background: &StoryBackground) -> gpui_kit::Div {
    match background {
        StoryBackground::Color(color) => div().bg(argb(*color)),
        StoryBackground::Gradient { colors, positions, angle } => {
            let stops: Vec<(Hsla, f32)> = colors.iter().zip(positions).map(|(c, p)| (argb(*c), p.clamp(0., 1.))).collect();
            let vertical = angle % 180 == 0;
            if stops.len() > 2 && vertical {
                // Top to bottom at 180°, bottom to top at 0°.
                let mut stops = stops;
                if angle % 360 == 0 {
                    stops.reverse();
                    for stop in &mut stops {
                        stop.1 = 1. - stop.1;
                    }
                }
                div().flex().flex_col().children(stops.windows(2).map(|pair| {
                    let ((from, start), (to, end)) = (pair[0], pair[1]);
                    div().w_full().flex_basis(px(0.)).flex_grow((end - start).max(0.001)).bg(linear_gradient(
                        180.,
                        linear_color_stop(from, 0.),
                        linear_color_stop(to, 1.),
                    ))
                }))
            } else {
                let first = stops.first().copied().unwrap_or((argb(0xffffffff), 0.));
                let last = stops.last().copied().unwrap_or(first);
                div().bg(linear_gradient(*angle as f32, linear_color_stop(first.0, first.1), linear_color_stop(last.0, last.1)))
            }
        }
    }
}

/// The font for a text story's style (Desktop bundles Inter, EB Garamond,
/// Parisienne and Barlow Condensed; these are macOS's nearest).
fn font_family(font: StoryFont) -> &'static str {
    match font {
        StoryFont::Default | StoryFont::Regular | StoryFont::Bold => ".SystemUIFont",
        StoryFont::Serif => "Georgia",
        StoryFont::Script => "Snell Roundhand",
        StoryFont::Condensed => "Avenir Next Condensed",
    }
}

/// Desktop's text size, by length (on the 720 × 1280 canvas).
fn font_size(text: &str) -> f32 {
    let graphemes = text.chars().count();
    if graphemes < 50 {
        59.
    } else if graphemes < 200 {
        42.
    } else {
        32.
    }
}

/// A text story drawn `height` tall (Desktop's `TextAttachment`).
pub fn render_text_story(story: &TextStory, height: Pixels) -> AnyElement {
    let scale = f32::from(height) / CANVAS_HEIGHT;
    let size = font_size(&story.text);
    // Desktop's default text color: white.
    let foreground = story.foreground.map(argb).unwrap_or(Hsla { h: 0., s: 0., l: 1., a: 1. });
    let small = size < 40.;
    let text = div()
        .max_w_full()
        .font_family(font_family(story.font))
        .when(story.font == StoryFont::Bold, |d| d.font_weight(FontWeight::BOLD))
        .text_size(px(size * scale))
        .line_height(px(size * 1.2 * scale))
        .text_color(foreground)
        .when(!small, |d| d.text_center())
        .when_some(story.text_background, |d, bg| {
            d.bg(argb(bg)).rounded(px(36. * scale)).p(px(28. * scale))
        })
        .child(story.text.clone());
    story_background(&story.background)
        .h(height)
        .w(px(CANVAS_WIDTH * scale))
        .relative()
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(24. * scale))
                .p(px(48. * scale))
                .child(text)
                .when_some(story.preview.as_ref(), |d, preview| {
                    d.child(
                        div()
                            .max_w(px(656. * scale))
                            .w_full()
                            .p(px(24. * scale))
                            .rounded(px(18. * scale))
                            .bg(Hsla { h: 0., s: 0., l: 1., a: 0.9 })
                            .text_color(Hsla { h: 0., s: 0., l: 0., a: 1. })
                            .text_size(px(28. * scale))
                            .child(preview.title.clone().unwrap_or_else(|| preview.url.clone())),
                    )
                }),
        )
        .into_any_element()
}

/// A small picture of a story: its photo, or its text story.
pub fn thumbnail(story: &Story, palette: Palette) -> AnyElement {
    let frame = div().flex_none().w(px(THUMB_WIDTH)).h(px(THUMB_HEIGHT)).rounded(px(8.)).overflow_hidden();
    match &story.content {
        StoryContent::Text(text) => frame.child(render_text_story(text, px(THUMB_HEIGHT))).into_any_element(),
        StoryContent::Media { attachment, .. } => match &attachment.local_path {
            Some(path) if attachment.content_type.starts_with("image/") => frame
                .child(img(std::path::PathBuf::from(path)).size_full().object_fit(ObjectFit::Cover))
                .into_any_element(),
            _ => frame.bg(palette.row_hover).into_any_element(),
        },
    }
}

/// The avatar with Desktop's ring: blue while there's something unviewed.
fn ringed(avatar: AnyElement, unread: bool, palette: Palette) -> impl IntoElement {
    div()
        .flex_none()
        .p(px(3.))
        .rounded_full()
        .border_2()
        .border_color(if unread { gpui_kit::rgb(0x2c58c3).into() } else { Hsla { a: 0.4, ..palette.text_secondary } })
        .child(avatar)
}

pub struct StoriesPane {
    state: Entity<AppState>,
    show_hidden: bool,
}

impl StoriesPane {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { state, show_hidden: false }
    }

    fn render_mine(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mine = self.state.read(cx).my_stories();
        let latest = mine.last().cloned();
        let has_any = latest.is_some();
        let views: usize = mine.iter().map(|s| s.views.len()).sum();
        let state = self.state.clone();
        let me = self.state.read(cx).account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
        let photo = self.state.read(cx).photo_of(&me).map(std::path::Path::to_path_buf);
        div()
            .id("my-stories")
            .debug_selector(|| "my-stories".into())
            .mx_2()
            .px(px(10.))
            .h(px(96.))
            .flex()
            .items_center()
            .gap_3()
            .rounded(px(10.))
            .cursor_pointer()
            .hover(|d| d.bg(palette.row_hover))
            // Desktop: with none up, it adds a text story.
            .on_click(move |_, _, cx| {
                state.update(cx, |s, cx| if has_any { s.view_stories(None, cx) } else { s.create_text_story(cx) })
            })
            .child(crate::views::common::person_avatar(photo.as_deref(), "Me", crate::theme::color_for(&me), px(48.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(if mine.is_empty() { "My Story" } else { "My Stories" }))
                    .child(div().text_sm().text_color(palette.text_secondary).child(match &latest {
                        None => "Add a story".to_owned(),
                        Some(story) if views > 0 => format!("{} · {views} views", short_timestamp(story.sent_at)),
                        Some(story) => short_timestamp(story.sent_at),
                    })),
            )
            .children(latest.map(|s| thumbnail(&s, palette)))
    }

    /// Desktop's add-story button: "Text story" or "Photo or video".
    fn render_add_story(&self) -> impl IntoElement {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::DropdownMenu as _;
        let state = self.state.clone();
        Button::new("add-story")
            .ghost()
            .small()
            .icon(gpui_kit::assets::IconName::Plus)
            .tooltip("Add a story")
            .dropdown_menu(move |menu, _, _| {
                let (text_state, media_state) = (state.clone(), state.clone());
                menu.item(PopupMenuItem::new("Text story").on_click(move |_, _, cx| {
                    text_state.update(cx, |s, cx| s.create_text_story(cx));
                }))
                .item(PopupMenuItem::new("Photo or video").on_click(move |_, window, cx| {
                    let state = media_state.clone();
                    let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: false,
                        prompt: Some("Add to story".into()),
                    });
                    window
                        .spawn(cx, async move |cx| {
                            if let Ok(Ok(Some(paths))) = picked.await
                                && let Some(path) = paths.into_iter().next()
                            {
                                state.update(cx, |s, cx| s.create_media_story(path, cx));
                            }
                        })
                        .detach();
                }))
            })
    }

    fn render_group(&self, group: &StoryGroup, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state.read(cx);
        let unread = group.unread();
        let latest = group.stories.last().cloned();
        let avatar = match state.conversation(&group.chat) {
            Some(chat) => conversation_avatar(chat, px(48.)),
            None => crate::views::common::avatar(&group.title, crate::theme::color_for(&group.chat.0), px(48.)).into_any_element(),
        };
        let replied = group.stories.iter().any(|s| !s.replies.is_empty());
        let chat = group.chat.clone();
        let (menu_chat, hidden) = (group.chat.clone(), group.hidden);
        let open_state = self.state.clone();
        let menu_state = self.state.clone();
        let id = SharedString::from(format!("story-row-{}", group.chat.0));
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .mx_2()
            .px(px(10.))
            .h(px(96.))
            .flex()
            .items_center()
            .gap_3()
            .rounded(px(10.))
            .cursor_pointer()
            .hover(|d| d.bg(palette.row_hover))
            .on_click(move |_, _, cx| open_state.update(cx, |s, cx| s.view_stories(Some(chat.clone()), cx)))
            .child(ringed(avatar, unread, palette))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(group.title.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .text_color(palette.text_secondary)
                            .child(latest.as_ref().map(|s| short_timestamp(s.sent_at)).unwrap_or_default())
                            .when(replied, |d| {
                                d.child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::MessageCircle).size_3())
                            }),
                    ),
            )
            .children(latest.map(|s| thumbnail(&s, palette)))
            .context_menu(move |menu, _, _| {
                let (state, chat) = (menu_state.clone(), menu_chat.clone());
                let (go_state, go_chat) = (menu_state.clone(), menu_chat.clone());
                menu.item(PopupMenuItem::new(if hidden { "Unhide story" } else { "Hide story" }).on_click(move |_, _, cx| {
                    state.update(cx, |s, cx| s.set_story_hidden(chat.clone(), !hidden, cx));
                }))
                .item(PopupMenuItem::new("Go to chat").on_click(move |_, _, cx| {
                    go_state.update(cx, |s, cx| {
                        s.set_tab(crate::state::NavTab::Chats, cx);
                        s.select(Some(go_chat.clone()), cx);
                    });
                }))
            })
            .into_any_element()
    }
}

impl Render for StoriesPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let collapsed = self.state.read(cx).nav_collapsed;
        let groups = self.state.read(cx).story_groups();
        let (hidden, shown): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| g.hidden);
        let show_hidden = self.show_hidden;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.sidebar)
            .text_color(palette.text)
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pr_4()
                    // Clear the macOS traffic lights while the tabs are hidden.
                    .when(collapsed, |d| d.mt(crate::views::common::title_bar_inset(window)))
                    .pl(px(16.))
                    .when(collapsed, |d| d.child(crate::views::nav::show_tabs_button(&self.state, palette, cx)))
                    .child(div().flex_1().text_lg().font_weight(FontWeight::SEMIBOLD).child("Stories"))
                    .child(self.render_add_story()),
            )
            .child(
                div()
                    .id("stories-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_mine(palette, cx))
                    .children(shown.iter().map(|g| self.render_group(g, palette, cx)).collect::<Vec<_>>())
                    .when(shown.is_empty(), |d| {
                        d.child(
                            div()
                                .px_4()
                                .py_8()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_1()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("No stories"))
                                .child(div().text_sm().text_color(palette.text_secondary).child("New updates will appear here.")),
                        )
                    })
                    .when(!hidden.is_empty(), |d| {
                        d.child(
                            div()
                                .id("hidden-stories")
                                .mx_2()
                                .mt_2()
                                .px(px(10.))
                                .py_2()
                                .flex()
                                .items_center()
                                .gap_1()
                                .rounded(px(10.))
                                .cursor_pointer()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(palette.text_secondary)
                                .hover(|d| d.bg(palette.row_hover))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_hidden = !this.show_hidden;
                                    cx.notify();
                                }))
                                .child(gpui_kit::component::Icon::new(if show_hidden {
                                    gpui_kit::assets::IconName::ChevronDown
                                } else {
                                    gpui_kit::assets::IconName::ChevronRight
                                }).size_4())
                                .child("Hidden stories"),
                        )
                        .when(show_hidden, |d| d.children(hidden.iter().map(|g| self.render_group(g, palette, cx)).collect::<Vec<_>>()))
                    }),
            )
    }
}

/// The right pane while no story is open (Desktop's placeholder).
pub fn render_placeholder(palette: Palette) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(palette.background)
        .text_color(palette.text_secondary)
        .child("Click to view a story")
}
