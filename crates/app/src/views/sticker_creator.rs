//! Desktop's sticker pack creator (File → Create/upload sticker pack), in a
//! window of its own as in Desktop (app/main.main.ts
//! `showStickerCreatorWindow`, sticker-creator/src/routes/art/*): add
//! stickers, give each an emoji, name the pack and pick its cover, confirm,
//! upload, then share the link. Nothing is saved as a draft; closing the
//! window drops the pack, as in Desktop.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{Disableable as _, Theme};
use gpui_kit::prelude::*;
use gpui_kit::{
    Anchor, App, ClipboardItem, Context, Entity, ExternalPaths, FontWeight, Global, Image, ImageFormat, ObjectFit,
    PathPromptOptions, SharedString, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, anchored, deferred, div, img, px, size,
};
use signal_backend::Command;
use signal_backend::sticker_upload::{Cover, MAX_STICKERS, NewSticker, NewStickerPack, StickerImage, StickerUpload};

use crate::state::{AppEvent, AppState};
use crate::sticker_image;
use crate::theme::Palette;
use crate::views::common::ClosedByClick;
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};

/// Desktop's "Sticker Pack Creator Guidelines" link.
const GUIDELINES: &str = "https://support.signal.org/hc/articles/360031836512-Stickers#sticker_creator";
const TILE: f32 = 160.;
const NOTICE_DISMISS: std::time::Duration = std::time::Duration::from_secs(10);
/// Desktop's recommended margin: 16px of 512.
const MARGIN: f32 = TILE * 16. / 512.;

struct CreatorWindow(gpui_kit::AnyWindowHandle);
impl Global for CreatorWindow {}

/// Opens the creator, or brings back the one already open.
pub fn open(state: Entity<AppState>, cx: &mut App) {
    if let Some(CreatorWindow(handle)) = cx.try_global::<CreatorWindow>()
        && handle.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }
    let bounds = gpui_kit::Bounds::centered(None, size(px(800.), px(815.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(800.), px(750.))),
        titlebar: Some(TitlebarOptions { title: Some("Sticker pack creator".into()), ..Default::default() }),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        window.observe_window_appearance(|window, cx| Theme::sync_system_appearance(Some(window), cx)).detach();
        cx.new(|cx| StickerCreator::new(state, window, cx))
    }) {
        Ok((handle, _)) => cx.set_global(CreatorWindow(handle)),
        Err(error) => tracing::error!("couldn't open the sticker pack creator: {error:#}"),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Drop,
    Emoji,
    Meta,
    Upload,
    Share,
}

enum Art {
    Processing,
    Ready { sticker: StickerImage, preview: Arc<Image> },
}

impl Art {
    fn ready(sticker: StickerImage) -> Self {
        let format = if sticker.content_type == "image/png" { ImageFormat::Png } else { ImageFormat::Webp };
        let preview = Arc::new(Image::from_bytes(format, sticker.bytes.clone()));
        Art::Ready { sticker, preview }
    }

    fn preview(&self) -> Option<Arc<Image>> {
        match self {
            Art::Ready { preview, .. } => Some(preview.clone()),
            Art::Processing => None,
        }
    }
}

struct Sticker {
    key: u64,
    /// The file it came from; dropping it again doesn't add it twice.
    path: PathBuf,
    art: Art,
    emoji: Option<String>,
}

/// A sticker being dragged to a new place in the pack.
#[derive(Clone)]
struct Dragged {
    key: u64,
    preview: Option<Arc<Image>>,
}

impl Render for Dragged {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(TILE / 2.)).opacity(0.8).when_some(self.preview.clone(), |d, p| {
            d.child(img(p).size_full().object_fit(ObjectFit::Contain))
        })
    }
}

pub struct StickerCreator {
    state: Entity<AppState>,
    stage: Stage,
    stickers: Vec<Sticker>,
    next_key: u64,
    show_margins: bool,
    picker: Entity<EmojiPicker>,
    /// The sticker whose emoji is being picked.
    picking_for: Option<u64>,
    picker_closed_by: ClosedByClick,
    title: Entity<InputState>,
    author: Entity<InputState>,
    /// A cover that isn't one of the stickers (`None`: the first sticker).
    cover: Option<Art>,
    confirming: bool,
    /// Desktop's toasts: what was added, or why something wasn't.
    notice: Option<SharedString>,
    /// Counts notices, so an old one's timer doesn't clear a newer one.
    notice_count: u64,
    /// Images added while "N images added" shows; Desktop counts on in
    /// the toast that's up.
    added: usize,
    progress: (usize, usize),
    url: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl StickerCreator {
    fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
        picker.update(cx, |picker, cx| picker.set_media(false, window, cx));
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Name your sticker pack"));
        let author = cx.new(|cx| InputState::new(window, cx).placeholder("Enter a name to submit your stickers under"));
        let _subscriptions = vec![
            cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, _, cx| {
                if let (PickerEvent::Picked(glyph), Some(key)) = (event, this.picking_for)
                    && let Some(sticker) = this.stickers.iter_mut().find(|s| s.key == key)
                {
                    sticker.emoji = Some(glyph.clone());
                }
                this.picking_for = None;
                cx.notify();
            }),
            cx.subscribe(&title, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.subscribe(&author, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.subscribe(&state, |this, _, event: &AppEvent, cx| {
                if let AppEvent::StickerUpload(upload) = event {
                    this.on_upload(upload.clone(), cx);
                }
            }),
        ];
        Self {
            state,
            stage: Stage::Drop,
            stickers: Vec::new(),
            next_key: 0,
            show_margins: true,
            picker,
            picking_for: None,
            picker_closed_by: ClosedByClick::default(),
            title,
            author,
            cover: None,
            confirming: false,
            notice: None,
            notice_count: 0,
            added: 0,
            progress: (0, 0),
            url: None,
            _subscriptions,
        }
    }

    /// Shows a toast, which goes after Desktop's 10 seconds
    /// (`DEFAULT_DISMISS` in `sticker-creator/src/components/Toaster.tsx`)
    /// or on a click.
    fn show_notice(&mut self, notice: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.added = 0;
        self.notice = Some(notice.into());
        self.notice_count += 1;
        let count = self.notice_count;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_DISMISS).await;
            let _ = this.update(cx, |this, cx| {
                if this.notice_count == count {
                    this.clear_notice();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn clear_notice(&mut self) {
        self.notice = None;
        self.added = 0;
    }

    /// One more image added: "N images added", counting on while it shows.
    fn image_added(&mut self, cx: &mut Context<Self>) {
        let added = self.added + 1;
        let plural = if added == 1 { "" } else { "s" };
        self.show_notice(format!("{added} image{plural} added"), cx);
        self.added = added;
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stage = Stage::Drop;
        self.stickers.clear();
        self.cover = None;
        self.clear_notice();
        self.url = None;
        self.title.update(cx, |input, cx| input.set_value("", window, cx));
        self.author.update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    fn pick_files(&mut self, cover: bool, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: !cover,
            prompt: Some(if cover { "Choose" } else { "Add" }.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update(cx, |this, cx| {
                    if cover {
                        this.set_cover(paths.into_iter().next(), cx);
                    } else {
                        this.add(paths, cx);
                    }
                });
            }
        })
        .detach();
    }

    /// Adds images in order, skipping files already in the pack, up to
    /// Desktop's 200.
    fn add(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for path in paths {
            if self.stickers.len() >= MAX_STICKERS {
                break;
            }
            if self.stickers.iter().any(|s| s.path == path) {
                continue;
            }
            if !sticker_image::accepts(&path) {
                self.show_notice(sticker_image::Rejected::Unsupported.message(), cx);
                continue;
            }
            let key = self.next_key;
            self.next_key += 1;
            self.stickers.push(Sticker { key, path: path.clone(), art: Art::Processing, emoji: None });
            let task = cx.background_spawn(process(path));
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok(sticker) => {
                            if let Some(s) = this.stickers.iter_mut().find(|s| s.key == key) {
                                s.art = Art::ready(sticker);
                                // Desktop's `adjustCover`: the first
                                // sticker is the cover again.
                                this.cover = None;
                                this.image_added(cx);
                            }
                        }
                        Err(rejected) => {
                            this.stickers.retain(|s| s.key != key);
                            this.show_notice(rejected.message(), cx);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn set_cover(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(path) = path else { return };
        // A file of the wrong type, as Desktop's cover drop zone refuses it
        // (`onDropRejected` in `MetaStage.tsx`).
        if !sticker_image::accepts(&path) {
            return self.show_notice("Error processing image", cx);
        }
        self.cover = Some(Art::Processing);
        let task = cx.background_spawn(process(path));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(sticker) => this.cover = Some(Art::ready(sticker)),
                    Err(rejected) => {
                        this.cover = None;
                        this.show_notice(rejected.message(), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Puts the dragged sticker where `target` is.
    fn move_to(&mut self, dragged: u64, target: u64, cx: &mut Context<Self>) {
        let (Some(from), Some(to)) = (
            self.stickers.iter().position(|s| s.key == dragged),
            self.stickers.iter().position(|s| s.key == target),
        ) else {
            return;
        };
        let sticker = self.stickers.remove(from);
        self.stickers.insert(to, sticker);
        cx.notify();
    }

    fn all_ready(&self) -> bool {
        !self.stickers.is_empty() && self.stickers.iter().all(|s| matches!(s.art, Art::Ready { .. }))
    }

    fn can_continue(&self, cx: &App) -> bool {
        let filled = |input: &Entity<InputState>| !input.read(cx).value().is_empty();
        match self.stage {
            Stage::Drop => self.all_ready(),
            Stage::Emoji => self.all_ready() && self.stickers.iter().all(|s| s.emoji.is_some()),
            Stage::Meta => {
                self.all_ready()
                    && self.stickers.iter().all(|s| s.emoji.is_some())
                    && !matches!(self.cover, Some(Art::Processing))
                    && filled(&self.title)
                    && filled(&self.author)
            }
            Stage::Upload | Stage::Share => false,
        }
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        if !self.can_continue(cx) {
            return;
        }
        self.clear_notice();
        match self.stage {
            Stage::Drop => self.stage = Stage::Emoji,
            Stage::Emoji => self.stage = Stage::Meta,
            Stage::Meta => self.confirming = true,
            Stage::Upload | Stage::Share => {}
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.clear_notice();
        self.picking_for = None;
        self.stage = match self.stage {
            Stage::Emoji => Stage::Drop,
            Stage::Meta => Stage::Emoji,
            stage => stage,
        };
        cx.notify();
    }

    fn upload(&mut self, cx: &mut Context<Self>) {
        self.confirming = false;
        let stickers: Vec<NewSticker> = self
            .stickers
            .iter()
            .filter_map(|s| match &s.art {
                Art::Ready { sticker, .. } => Some(NewSticker { image: sticker.clone(), emoji: s.emoji.clone()? }),
                Art::Processing => None,
            })
            .collect();
        if stickers.len() != self.stickers.len() {
            return;
        }
        let cover = match &self.cover {
            Some(Art::Ready { sticker, .. }) => Cover::Image(sticker.clone()),
            _ => Cover::Sticker(0),
        };
        let pack = NewStickerPack {
            title: self.title.read(cx).value().to_string(),
            author: self.author.read(cx).value().to_string(),
            stickers,
            cover,
        };
        self.progress = (0, pack.stickers.len());
        self.stage = Stage::Upload;
        self.state.read(cx).send(Command::UploadStickerPack(pack));
        cx.notify();
    }

    fn on_upload(&mut self, upload: StickerUpload, cx: &mut Context<Self>) {
        if self.stage != Stage::Upload {
            return;
        }
        match upload {
            StickerUpload::Progress { done, total } => self.progress = (done, total),
            StickerUpload::Done { url, .. } => {
                self.url = Some(url);
                self.stage = Stage::Share;
            }
            StickerUpload::Failed(error) => {
                self.stage = Stage::Meta;
                self.show_notice(format!("Error uploading images: {error}"), cx);
            }
        }
        cx.notify();
    }

    fn render_tile(&self, ix: usize, sticker: &Sticker, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let key = sticker.key;
        let preview = sticker.art.preview();
        let stage = self.stage;
        div()
            .id(("sticker", key as usize))
            .debug_selector(move || format!("sticker-tile-{ix}"))
            .relative()
            .size(px(TILE))
            .rounded_lg()
            .bg(palette.input_bg)
            .border_2()
            .border_color(palette.input_bg)
            .flex()
            .items_center()
            .justify_center()
            .map(|d| match preview.clone() {
                // The sticker's own 512px square, edge to edge, so the guide
                // shows Desktop's margin.
                Some(preview) => d.child(img(preview).size_full().object_fit(ObjectFit::Contain)),
                None => d.child(div().text_sm().text_color(palette.text_secondary).child("Processing…")),
            })
            .when(stage == Stage::Drop && self.show_margins, |d| {
                d.child(
                    div()
                        .absolute()
                        .inset(px(MARGIN))
                        .border_1()
                        .border_color(palette.text_secondary.opacity(0.4)),
                )
            })
            .when(stage == Stage::Drop, |d| {
                d.cursor_grab()
                    .on_drag(Dragged { key, preview: preview.clone() }, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                    .drag_over::<Dragged>(move |style, _, _, _| style.border_color(palette.accent))
                    .on_drop(cx.listener(move |this, dragged: &Dragged, _, cx| this.move_to(dragged.key, key, cx)))
                    .child(
                        div()
                            .id(("remove-sticker", key as usize))
                            .absolute()
                            .top_1()
                            .right_1()
                            .size(px(24.))
                            .rounded_full()
                            .bg(palette.popover)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(palette.text)
                            .cursor_pointer()
                            .hover(|d| d.bg(palette.row_hover))
                            .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Remove image").build(window, cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.stickers.retain(|s| s.key != key);
                                // Desktop's `adjustCover`.
                                this.cover = None;
                                cx.notify();
                            }))
                            .child("✕"),
                    )
            })
            .when(stage == Stage::Emoji, |d| {
                let open = self.picking_for == Some(key);
                d.child(
                    div()
                        .absolute()
                        .bottom_1()
                        .right_1()
                        .child(
                            div()
                                .id(("sticker-emoji", key as usize))
                                .debug_selector(move || format!("sticker-emoji-{ix}"))
                                .size(px(36.))
                                .rounded_full()
                                .bg(palette.popover)
                                .border_1()
                                .border_color(palette.panel_border)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(20.))
                                .cursor_pointer()
                                .hover(|d| d.bg(palette.row_hover))
                                .on_click(cx.listener(move |this, click, window, cx| {
                                    if this.picker_closed_by.take(click) {
                                        return;
                                    }
                                    this.picking_for = if this.picking_for == Some(key) { None } else { Some(key) };
                                    this.picker.update(cx, |picker, cx| picker.reset(window, cx));
                                    cx.notify();
                                }))
                                .map(|d| match &sticker.emoji {
                                    Some(emoji) => d.child(emoji.clone()),
                                    None => d.child(
                                        gpui_kit::component::Icon::new(gpui_kit::assets::IconName::FaceSlightlySmilingPlus)
                                            .size_5()
                                            .text_color(palette.text_secondary),
                                    ),
                                }),
                        )
                        .when(open, |d| {
                            d.child(deferred(
                                anchored()
                                    .anchor(Anchor::TopRight)
                                    .snap_to_window_with_margin(px(8.))
                                    .child(
                                        div()
                                            .pt_1()
                                            .on_mouse_down_out(cx.listener(|this, event, _, cx| {
                                                this.picker_closed_by.record(event);
                                                this.picking_for = None;
                                                cx.notify();
                                            }))
                                            .child(self.picker.clone()),
                                    ),
                            ))
                        }),
                )
            })
    }

    fn render_grid(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tiles: Vec<_> = self
            .stickers
            .iter()
            .enumerate()
            .map(|(ix, s)| self.render_tile(ix, s, palette, cx).into_any_element())
            .collect();
        let can_add = self.stage == Stage::Drop && self.stickers.len() < MAX_STICKERS;
        div().flex().flex_wrap().gap_3().children(tiles).when(can_add, |d| {
            d.child(
                div()
                    .id("add-sticker")
                    .debug_selector(|| "add-sticker".into())
                    .size(px(TILE))
                    .rounded_lg()
                    .border_2()
                    .border_color(palette.panel_border)
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .px_3()
                    .cursor_pointer()
                    .hover(|d| d.bg(palette.row_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.pick_files(false, window, cx)))
                    .child(div().text_size(px(32.)).text_color(palette.text_secondary).child("+"))
                    .child(
                        div()
                            .text_xs()
                            .text_center()
                            .text_color(palette.text_secondary)
                            .child("Click or drag/drop a file to add a sticker"),
                    ),
            )
        })
    }

    fn render_drop_zone(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("drop-zone")
            .debug_selector(|| "drop-zone".into())
            .h(px(360.))
            .rounded_xl()
            .border_2()
            .border_color(palette.panel_border)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|d| d.bg(palette.row_hover))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(palette.accent))
            .on_click(cx.listener(|this, _, window, cx| this.pick_files(false, window, cx)))
            .child(div().text_color(palette.text_secondary).child("Click to add or drop images here"))
    }

    fn render_meta(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let label = |text: &'static str| div().text_sm().font_weight(FontWeight::SEMIBOLD).child(text);
        let cover = match &self.cover {
            Some(art) => art.preview(),
            None => self.stickers.first().and_then(|s| s.art.preview()),
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().flex().flex_col().gap_2().child(label("Title")).child(Input::new(&self.title)))
            .child(div().flex().flex_col().gap_2().child(label("Author")).child(Input::new(&self.author)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label("Cover image"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.text_secondary)
                            .child("This is the image that will show up when you share your sticker pack"),
                    )
                    .child(
                        div()
                            .id("cover")
                            .debug_selector(|| "sticker-cover".into())
                            .size(px(TILE))
                            .rounded_lg()
                            .bg(palette.input_bg)
                            .border_2()
                            .border_color(palette.input_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|d| d.border_color(palette.panel_border))
                            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(palette.accent))
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                this.set_cover(paths.paths().first().cloned(), cx)
                            }))
                            .on_click(cx.listener(|this, _, window, cx| this.pick_files(true, window, cx)))
                            .map(|d| match cover {
                                Some(cover) => d.child(img(cover).size_full().object_fit(ObjectFit::Contain)),
                                None => d.child(div().text_sm().text_color(palette.text_secondary).child("Processing…")),
                            }),
                    ),
            )
    }

    fn render_upload(&self, palette: Palette) -> impl IntoElement {
        let (done, total) = self.progress;
        let fraction = if total == 0 { 0. } else { done as f32 / total as f32 };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "sticker-upload-progress".into())
            .child(div().text_color(palette.text_secondary).child(format!("{done} of {total} uploaded")))
            .child(Progress::new("sticker-upload").color(palette.accent).value(fraction * 100.))
    }

    fn render_share(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.url.clone().unwrap_or_default();
        let text = format!("Check out this new sticker pack I created for Signal. #makeprivacystick {url}");
        let encode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let shares = [
            ("share-facebook", "Facebook", format!("https://www.facebook.com/sharer/sharer.php?u={}", encode(&url))),
            ("share-x", "X", format!("https://x.com/intent/post?text={}", encode(&text))),
            ("share-whatsapp", "WhatsApp", format!("https://wa.me?text={}", encode(&text))),
        ];
        let previews: Vec<_> = self.stickers.iter().filter_map(|s| s.art.preview()).collect();
        let (title, author) = (self.title.read(cx).value().to_string(), self.author.read(cx).value().to_string());
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_xl()
                    .bg(palette.input_bg)
                    .child(div().text_xs().text_color(palette.text_secondary).child("Sticker pack"))
                    .child(div().flex().flex_wrap().gap_2().children(
                        previews.into_iter().map(|p| img(p).size(px(56.)).object_fit(ObjectFit::Contain)),
                    ))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_sm().text_color(palette.text_secondary).child(author)),
            )
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Sticker Pack URL"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(palette.input_bg)
                            .text_sm()
                            .debug_selector(|| "sticker-pack-url".into())
                            .child(url.clone()),
                    )
                    .child(Button::new("copy-url").label("Copy").outline().on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        this.show_notice("Link copied", cx);
                        cx.notify();
                    }))),
            )
            .child(div().text_sm().text_color(palette.text_secondary).child(
                "Use the hashtag #makeprivacystick to help other people find the URLs for any custom sticker \
                 packs that you would like to make publicly accessible.",
            ))
            .child(div().flex().gap_2().children(shares.into_iter().map(|(id, label, link)| {
                Button::new(id).label(label).outline().on_click(move |_, _, cx| cx.open_url(&link))
            })))
    }
}

async fn process(path: PathBuf) -> Result<StickerImage, sticker_image::Rejected> {
    let bytes = std::fs::read(&path).map_err(|_| sticker_image::Rejected::Unreadable)?;
    sticker_image::process(&path, &bytes)
}

impl Render for StickerCreator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let (title, help): (&str, Option<&str>) = match self.stage {
            Stage::Drop => (
                "Add your stickers",
                Some(
                    "Stickers must be in PNG, APNG, or WebP format with a transparent background and 512x512 \
                     pixels. Recommended margin is 16px.",
                ),
            ),
            Stage::Emoji => {
                ("Add an emoji to each sticker", Some("This allows us to suggest stickers to you as you're messaging."))
            }
            Stage::Meta => ("Just a few more details...", None),
            Stage::Upload => ("Creating your sticker pack", None),
            Stage::Share => (
                "Congratulations! You created a sticker pack.",
                Some(
                    "Access your new stickers through the sticker icon, or share with your friends using the \
                     link below.",
                ),
            ),
        };
        let can_continue = self.can_continue(cx);
        let stage = self.stage;
        let body = match stage {
            Stage::Drop if self.stickers.is_empty() => self.render_drop_zone(palette, cx).into_any_element(),
            Stage::Drop | Stage::Emoji => self.render_grid(palette, cx).into_any_element(),
            Stage::Meta => self.render_meta(palette, cx).into_any_element(),
            Stage::Upload => self.render_upload(palette).into_any_element(),
            Stage::Share => self.render_share(palette, cx).into_any_element(),
        };

        div()
            .id("sticker-creator")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .when(stage == Stage::Drop, |d| {
                d.on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.add(paths.paths().to_vec(), cx)))
            })
            // Header: Desktop's PageHeader.
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(palette.panel_border)
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child("Signal Sticker Pack Creator"))
                    .child(
                        div()
                            .id("guidelines")
                            .text_sm()
                            .text_color(palette.accent)
                            .cursor_pointer()
                            .on_click(|_, _, cx| cx.open_url(GUIDELINES))
                            .child("Sticker Pack Creator Guidelines"),
                    ),
            )
            .child(
                div()
                    .id("sticker-creator-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
                    .when_some(help, |d, help| d.child(div().text_sm().text_color(palette.text_secondary).child(help)))
                    .child(body),
            )
            .when_some(self.notice.clone(), |d, notice| {
                d.child(
                    div()
                        .id("sticker-notice")
                        .debug_selector(|| "sticker-notice".into())
                        .flex_none()
                        .mx_6()
                        .mb_2()
                        .px_4()
                        .py_2()
                        .rounded_lg()
                        .bg(palette.popover)
                        .border_1()
                        .border_color(palette.panel_border)
                        .text_sm()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_notice();
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            // Footer: Desktop's AppStage footer.
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_6()
                    .py_4()
                    .border_t_1()
                    .border_color(palette.panel_border)
                    .when(stage == Stage::Drop && !self.stickers.is_empty(), |d| {
                        d.child(Checkbox::new("view-margins").label("View margins").checked(self.show_margins).on_click(
                            cx.listener(|this, checked: &bool, _, cx| {
                                this.show_margins = *checked;
                                cx.notify();
                            }),
                        ))
                    })
                    .child(div().flex_1())
                    .when(matches!(stage, Stage::Emoji | Stage::Meta), |d| {
                        d.child(Button::new("back").label("Back").outline().on_click(cx.listener(|this, _, _, cx| this.back(cx))))
                    })
                    .when(matches!(stage, Stage::Drop | Stage::Emoji | Stage::Meta), |d| {
                        d.child(
                            Button::new("next")
                                .label("Next")
                                .primary()
                                .disabled(!can_continue)
                                .on_click(cx.listener(|this, _, _, cx| this.next(cx))),
                        )
                    })
                    .when(stage == Stage::Share, |d| {
                        d.child(
                            Button::new("create-another")
                                .label("Create another sticker pack")
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| this.reset(window, cx))),
                        )
                    }),
            )
            .when(self.confirming, |d| {
                // Desktop's ConfirmModal: a click outside cancels.
                d.child(
                    div()
                        .id("confirm-upload")
                        .absolute()
                        .inset_0()
                        .bg(gpui_kit::black().opacity(0.4))
                        .flex()
                        .items_center()
                        .justify_center()
                        .occlude()
                        .child(
                            div()
                                .w(px(400.))
                                .p_5()
                                .rounded_xl()
                                .bg(palette.popover)
                                .border_1()
                                .border_color(palette.panel_border)
                                .shadow_lg()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.confirming = false;
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Are you sure you want to upload your sticker pack?"),
                                )
                                .child(div().text_sm().text_color(palette.text_secondary).child(
                                    "You will no longer be able to make edits or delete after creating a sticker pack.",
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .justify_end()
                                        .gap_2()
                                        .child(Button::new("confirm-cancel").label("Cancel").outline().on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.confirming = false;
                                                cx.notify();
                                            }),
                                        ))
                                        .child(
                                            Button::new("confirm-upload-button")
                                                .label("Upload")
                                                .primary()
                                                .on_click(cx.listener(|this, _, _, cx| this.upload(cx))),
                                        ),
                                ),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc::UnboundedReceiver;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use signal_backend::{Backend, Config, Event};

    use super::*;

    struct Fixture {
        cx: VisualTestContext,
        view: Entity<StickerCreator>,
        state: Entity<AppState>,
        events: UnboundedReceiver<Event>,
        backend: Backend,
        dir: tempfile::TempDir,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.backend.shutdown();
        }
    }

    impl Fixture {
        fn new(cx: &mut TestAppContext) -> Self {
            cx.update(gpui_kit::init);
            cx.executor().allow_parking();
            let dir = tempfile::tempdir().unwrap();
            let (backend, events) = Backend::start(Config {
                data_dir: dir.path().join("data"),
                db_key: [7; 32],
                device_name: "test".into(),
                demo: true,
            })
            .unwrap();
            let (window, view, state) = cx.update(|cx| {
                let state = crate::state::new_state(backend.clone(), cx);
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                        origin: Default::default(),
                        size: size(px(800.), px(815.)),
                    })),
                    ..Default::default()
                };
                let for_view = state.clone();
                let (window, view) =
                    gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| StickerCreator::new(for_view, window, cx)))
                        .unwrap();
                (window, view, state)
            });
            Fixture { cx: VisualTestContext::from_window(window, cx), view, state, events, backend, dir }
        }

        /// A PNG of `width`×`height` in the fixture's directory.
        fn png(&self, name: &str, width: u32, height: u32) -> PathBuf {
            let path = self.dir.path().join(name);
            image::RgbaImage::from_pixel(width, height, image::Rgba([40, 120, 200, 255])).save(&path).unwrap();
            path
        }

        /// Runs until `done`, feeding backend events to the app as main.rs
        /// does.
        fn until(&mut self, what: &str, done: impl Fn(&StickerCreator, &App) -> bool) {
            for _ in 0..500 {
                while let Ok(event) = self.events.try_recv() {
                    let state = self.state.clone();
                    self.cx.update(|_, cx| state.update(cx, |state, cx| state.handle(event, cx)));
                }
                self.cx.run_until_parked();
                let view = self.view.clone();
                if self.cx.update(|_, cx| done(view.read(cx), cx)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("never {what}");
        }

        fn update<R>(&mut self, f: impl FnOnce(&mut StickerCreator, &mut Window, &mut Context<StickerCreator>) -> R) -> R {
            let view = self.view.clone();
            self.cx.update(|window, cx| view.update(cx, |this, cx| f(this, window, cx)))
        }
    }

    #[gpui_kit::test]
    fn a_pack_goes_from_images_to_a_share_link(cx: &mut TestAppContext) {
        let mut f = Fixture::new(cx);
        let (wide, square) = (f.png("wide.png", 300, 100), f.png("square.png", 64, 64));
        f.update(|this, _, cx| this.add(vec![wide.clone(), square.clone(), wide.clone()], cx));
        f.until("processed both", |this, _| this.all_ready() && this.stickers.len() == 2);
        assert_eq!(f.update(|this, _, _| this.notice.clone()), Some("2 images added".into()));

        // Next needs every image processed, then every emoji.
        f.update(|this, _, cx| this.next(cx));
        assert_eq!(f.update(|this, _, _| this.stage), Stage::Emoji);
        f.update(|this, _, cx| this.next(cx));
        assert_eq!(f.update(|this, _, _| this.stage), Stage::Emoji, "emojis missing");
        for emoji in ["🥾", "⛰️"] {
            f.update(|this, _, cx| {
                this.picking_for = this.stickers.iter().find(|s| s.emoji.is_none()).map(|s| s.key);
                this.picker.update(cx, |_, cx| cx.emit(PickerEvent::Picked(emoji.into())));
            });
            f.cx.run_until_parked();
        }
        f.update(|this, _, cx| this.next(cx));
        assert_eq!(f.update(|this, _, _| this.stage), Stage::Meta);

        // Title and author are required, then a confirmation.
        f.update(|this, _, cx| this.next(cx));
        assert!(!f.update(|this, _, _| this.confirming));
        f.update(|this, window, cx| {
            this.title.update(cx, |input, cx| input.set_value("Hikes", window, cx));
            this.author.update(cx, |input, cx| input.set_value("Me", window, cx));
        });
        f.update(|this, _, cx| this.next(cx));
        assert!(f.update(|this, _, _| this.confirming));

        f.update(|this, _, cx| this.upload(cx));
        assert_eq!(f.update(|this, _, _| (this.stage, this.progress)), (Stage::Upload, (0, 2)));
        f.until("shared", |this, _| this.stage == Stage::Share);
        assert_eq!(f.update(|this, _, _| this.progress), (2, 2));
        let url = f.update(|this, _, _| this.url.clone()).unwrap();
        assert!(url.starts_with("https://signal.art/addstickers/#pack_id=de71&pack_key="), "{url}");
    }

    #[gpui_kit::test]
    fn stickers_reorder_and_unsupported_files_are_refused(cx: &mut TestAppContext) {
        let mut f = Fixture::new(cx);
        let (a, b, c) = (f.png("a.png", 32, 32), f.png("b.png", 32, 32), f.png("c.png", 32, 32));
        let jpeg = f.dir.path().join("photo.jpg");
        std::fs::write(&jpeg, b"not really").unwrap();
        f.update(|this, _, cx| this.add(vec![a.clone(), b.clone(), c.clone(), jpeg], cx));
        f.until("processed", |this, _| this.all_ready() && this.stickers.len() == 3);
        let order = |f: &mut Fixture| f.update(|this, _, _| this.stickers.iter().map(|s| s.path.clone()).collect::<Vec<_>>());

        // Dragging the last onto the first puts it first.
        f.update(|this, _, cx| {
            let (last, first) = (this.stickers[2].key, this.stickers[0].key);
            this.move_to(last, first, cx);
        });
        assert_eq!(order(&mut f), [c.clone(), a.clone(), b.clone()]);

        // A sticker removed while the next drop processes stays removed.
        f.update(|this, _, cx| {
            let key = this.stickers[1].key;
            this.stickers.retain(|s| s.key != key);
            cx.notify();
        });
        assert_eq!(order(&mut f), [c, b]);
    }

    #[gpui_kit::test]
    fn the_cover_follows_desktops_rules_and_toasts_go_after_10_seconds(cx: &mut TestAppContext) {
        let mut f = Fixture::new(cx);
        let (a, b, cover) = (f.png("a.png", 32, 32), f.png("b.png", 32, 32), f.png("cover.png", 40, 40));
        f.update(|this, _, cx| this.add(vec![a], cx));
        f.until("processed", |this, _| this.all_ready());
        f.update(|this, _, cx| this.set_cover(Some(cover), cx));
        f.until("cover processed", |this, _| matches!(this.cover, Some(Art::Ready { .. })));

        // A file of the wrong type is refused as Desktop's cover drop zone
        // refuses it, and the cover stays.
        let jpeg = f.dir.path().join("photo.jpg");
        std::fs::write(&jpeg, b"not really").unwrap();
        f.update(|this, _, cx| this.set_cover(Some(jpeg), cx));
        assert_eq!(f.update(|this, _, _| this.notice.clone()), Some("Error processing image".into()));
        assert!(f.update(|this, _, _| matches!(this.cover, Some(Art::Ready { .. }))));

        // Adding a sticker makes the first sticker the cover again.
        f.update(|this, _, cx| this.add(vec![b], cx));
        f.until("second processed", |this, _| this.all_ready() && this.stickers.len() == 2);
        assert!(f.update(|this, _, _| this.cover.is_none()));
        assert_eq!(f.update(|this, _, _| this.notice.clone()), Some("1 image added".into()));

        // The toast goes by itself after 10 seconds.
        f.cx.executor().advance_clock(NOTICE_DISMISS);
        f.cx.run_until_parked();
        assert_eq!(f.update(|this, _, _| this.notice.clone()), None);
    }

    #[gpui_kit::test]
    fn a_failed_upload_goes_back_to_the_details(cx: &mut TestAppContext) {
        let mut f = Fixture::new(cx);
        f.update(|this, _, cx| {
            this.stage = Stage::Upload;
            this.on_upload(StickerUpload::Failed("no network".into()), cx);
        });
        assert_eq!(f.update(|this, _, _| this.stage), Stage::Meta);
        assert_eq!(f.update(|this, _, _| this.notice.clone()), Some("Error uploading images: no network".into()));

        // Answers that come after leaving the upload are ignored.
        f.update(|this, _, cx| this.on_upload(StickerUpload::Progress { done: 1, total: 2 }, cx));
        assert_eq!(f.update(|this, _, _| (this.stage, this.progress)), (Stage::Meta, (0, 0)));
    }
}
