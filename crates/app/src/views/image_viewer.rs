//! Full-window image viewer (Signal Desktop's lightbox): fit to the window,
//! zoom up to 100%, download to ~/Downloads.

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, Image, ImageFormat, KeyDownEvent, ObjectFit,
    SharedString, Window, div, img, px,
};

use crate::state::AppState;
use crate::views::lightbox::{self, InChat};

const TOOLBAR_HEIGHT: f32 = 52.;
const MARGIN: f32 = 24.;
/// Each zoom step multiplies the scale by this much.
const ZOOM_STEP: f32 = 1.25;

/// An image to show.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewedImage {
    pub path: PathBuf,
    /// Pixel size, when known; without it the image can only be fitted.
    pub size: Option<(u32, u32)>,
    pub file_name: Option<String>,
    /// Its stored MIME type, which "Copy Image" puts on the clipboard.
    pub content_type: String,
    /// A view-once message's copy (`Event::ViewOnceOpened`): it can't be
    /// saved, and the file is deleted when the viewer closes.
    pub view_once: bool,
    /// Its place in its chat, for the lightbox's previous and next,
    /// Forward and "Go to message".
    pub in_chat: Option<InChat>,
}

pub enum ViewerEvent {
    Close,
}

pub struct ImageViewer {
    image: ViewedImage,
    state: Option<Entity<AppState>>,
    /// `None`: fit the window. Otherwise a scale up to 1 (100%).
    zoom: Option<f32>,
    focus: FocusHandle,
}

impl EventEmitter<ViewerEvent> for ImageViewer {}

impl Focusable for ImageViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ImageViewer {
    pub fn new(image: ViewedImage, state: Option<Entity<AppState>>, cx: &mut Context<Self>) -> Self {
        if image.view_once {
            delete_on_release(image.path.clone(), cx);
        }
        if let (Some(state), Some(in_chat)) = (&state, image.in_chat.as_ref().filter(|c| c.gallery.is_none())) {
            let this = cx.weak_entity();
            lightbox::load(state, in_chat.conversation.clone(), cx, move |items, cx| {
                let _ = this.update(cx, |this, cx| {
                    if let Some(in_chat) = &mut this.image.in_chat {
                        in_chat.gallery = Some(std::rc::Rc::new(items));
                    }
                    cx.notify();
                });
            });
        }
        Self { image, state, zoom: None, focus: cx.focus_handle() }
    }

    /// Desktop's previous and next (not while zoomed in).
    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.zoom.is_some() {
            return;
        }
        let (Some(state), Some(in_chat)) = (&self.state, &self.image.in_chat) else { return };
        if let Some(item) = in_chat.step(by) {
            lightbox::show(state, in_chat, &item, cx);
        }
    }

    fn can_step(&self, by: isize) -> bool {
        self.zoom.is_none() && self.state.is_some() && self.image.in_chat.as_ref().is_some_and(|c| c.step(by).is_some())
    }

    /// Forward, or going to its message: the lightbox closes first.
    fn leave_for(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(state), Some(in_chat)) = (self.state.clone(), self.image.in_chat.clone()) else { return };
        cx.emit(ViewerEvent::Close);
        if forward {
            lightbox::forward(&state, &in_chat, window, cx);
        } else {
            lightbox::go_to(&state, &in_chat, cx);
        }
    }

    fn fit_scale(&self, area: (f32, f32)) -> f32 {
        fit_scale(self.image.size, area)
    }

    fn scale(&self, area: (f32, f32)) -> f32 {
        self.zoom.unwrap_or_else(|| self.fit_scale(area))
    }

    fn zoom_by(&mut self, factor: f32, area: (f32, f32), cx: &mut Context<Self>) {
        if self.image.size.is_none() {
            return;
        }
        let fit = self.fit_scale(area);
        let next = (self.scale(area) * factor).clamp(fit, 1.);
        // Back at fit: follow the window again.
        self.zoom = if (next - fit).abs() < 0.001 { None } else { Some(next) };
        cx.notify();
    }

    fn actual_size(&mut self, cx: &mut Context<Self>) {
        if self.image.size.is_some() {
            self.zoom = Some(1.);
            cx.notify();
        }
    }

    fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        if self.zoom.is_some() {
            self.zoom = None;
            cx.notify();
        } else {
            self.actual_size(cx);
        }
    }

    /// Desktop's Save: a save dialog.
    fn save(&self, cx: &mut Context<Self>) {
        lightbox::save_as(self.image.path.clone(), download_name(&self.image), cx);
    }

    /// Desktop's "Copy Image" (`app/spell_check.main.ts`): the file's bytes,
    /// typed by its stored content type.
    fn copy(&self, cx: &mut Context<Self>) {
        copy_image(self.image.path.clone(), &self.image.content_type, cx);
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let area = available_area(window);
        // Desktop's lightbox: ⌘S saves the one shown (not a view-once copy).
        if event.keystroke.modifiers.platform && event.keystroke.key == "s" {
            if !self.image.view_once {
                self.save(cx);
            }
            return cx.stop_propagation();
        }
        match event.keystroke.key.as_str() {
            "escape" => cx.emit(ViewerEvent::Close),
            "=" | "+" => self.zoom_by(ZOOM_STEP, area, cx),
            "-" => self.zoom_by(1. / ZOOM_STEP, area, cx),
            "0" => self.actual_size(cx),
            "left" => self.step(-1, cx),
            "right" => self.step(1, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

/// Deletes a view-once copy once its viewer is gone, as Desktop's lightbox
/// deletes its temporary file on close (`ts/state/ducks/lightbox.preload.ts`).
pub fn delete_on_release<V: 'static>(path: PathBuf, cx: &mut Context<V>) {
    cx.on_release(move |_, _| {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(%error, "couldn't delete a view-once copy");
        }
    })
    .detach();
}

/// Puts the image at `path` on the clipboard as `content_type`. A type the
/// clipboard can't carry is logged and skipped.
pub fn copy_image(path: PathBuf, content_type: &str, cx: &mut App) {
    let Some(format) = ImageFormat::from_mime_type(content_type) else {
        tracing::warn!(content_type, "can't copy an image of this type");
        return;
    };
    let read = cx.background_spawn(async move { std::fs::read(&path) });
    cx.spawn(async move |cx| match read.await {
        Ok(bytes) => cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(format, bytes)))),
        Err(error) => tracing::warn!(%error, "failed to read an image to copy"),
    })
    .detach();
}

/// The scale that fits an image in `area`, never above 100%.
fn fit_scale(size: Option<(u32, u32)>, area: (f32, f32)) -> f32 {
    match size {
        Some((w, h)) if w > 0 && h > 0 => (area.0 / w as f32).min(area.1 / h as f32).min(1.),
        _ => 1.,
    }
}

fn available_area(window: &Window) -> (f32, f32) {
    let size = window.viewport_size();
    (f32::from(size.width) - MARGIN * 2., f32::from(size.height) - TOOLBAR_HEIGHT - MARGIN * 2.)
}

/// The sent file name, else `signal-<time>.<ext>` like Desktop.
fn download_name(image: &ViewedImage) -> String {
    if let Some(name) = image.file_name.as_deref().map(sanitize).filter(|n| !n.is_empty()) {
        return name;
    }
    let ext = image.path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "jpg".into());
    format!("signal-{}.{ext}", chrono::Local::now().format("%Y-%m-%d-%H%M%S"))
}

/// A file name can't carry a path into the Downloads folder.
fn sanitize(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '\0')).collect::<String>().trim_start_matches('.').to_owned()
}

/// Copies `source` into `dir` as `name` (or `name (1)`, …), claiming the
/// name atomically so a file that appears meanwhile is never overwritten.
pub fn save_copy(source: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let mut input = std::fs::File::open(source)?;
    for _ in 0..1000 {
        let target = unique_path(dir, name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(mut output) => {
                if let Err(error) = std::io::copy(&mut input, &mut output) {
                    // No truncated file left behind.
                    drop(output);
                    let _ = std::fs::remove_file(&target);
                    return Err(error);
                }
                return Ok(target);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

/// `name`, or `name (1)`, `name (2)`… if taken.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

impl gpui_kit::Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let area = available_area(window);
        let scale = self.scale(area);
        let (w, h) = match self.image.size {
            Some((w, h)) => (w as f32 * scale, h as f32 * scale),
            None => (area.0, area.1),
        };
        let percent: SharedString = format!("{:.0}%", scale * 100.).into();
        let can_zoom = self.image.size.is_some();
        let fit = self.fit_scale(area);
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        let in_chat = self.image.in_chat.as_ref().filter(|_| self.state.is_some() && !self.image.view_once);

        div()
            .id("image-viewer")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .bg(Hsla { h: 0., s: 0., l: 0., a: 0.92 })
            .text_color(white)
            .child(
                div()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    // Clear the macOS traffic lights.
                    .pl(px(92.))
                    .child(lightbox::header(self.image.in_chat.as_ref(), self.image.file_name.as_deref()))
                    .child(
                        Button::new("zoom-out")
                            .ghost()
                            .small()
                            .icon(IconName::Minus)
                            .tooltip("Zoom out (−)")
                            .disabled(!can_zoom || scale <= fit + 0.001)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.zoom_by(1. / ZOOM_STEP, available_area(window), cx)
                            })),
                    )
                    .child(div().w(px(48.)).flex().justify_center().text_sm().debug_selector(|| "zoom-level".into()).child(percent))
                    .child(
                        Button::new("zoom-in")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("Zoom in (+)")
                            .disabled(!can_zoom || scale >= 0.999)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.zoom_by(ZOOM_STEP, available_area(window), cx)
                            })),
                    )
                    .child(
                        Button::new("actual-size")
                            .ghost()
                            .small()
                            .label("Actual size")
                            .disabled(!can_zoom || scale >= 0.999)
                            .on_click(cx.listener(|this, _, _, cx| this.actual_size(cx))),
                    )
                    // Desktop's Forward and Save, which a view-once photo
                    // hasn't; and its message in the chat.
                    .when(in_chat.is_some(), |d| {
                        d.child(
                            Button::new("forward-image")
                                .ghost()
                                .small()
                                .icon(IconName::Forward)
                                .tooltip("Forward")
                                .on_click(cx.listener(|this, _, window, cx| this.leave_for(true, window, cx))),
                        )
                        .child(
                            Button::new("go-to-image-message")
                                .ghost()
                                .small()
                                .icon(IconName::MessageSquareShare)
                                .tooltip("Go to message")
                                .on_click(cx.listener(|this, _, window, cx| this.leave_for(false, window, cx))),
                        )
                    })
                    .when(!self.image.view_once, |d| d.child(
                        Button::new("download-image")
                            .ghost()
                            .small()
                            .icon(IconName::Download)
                            .tooltip("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ))
                    .child(
                        Button::new("close-image")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip("Close (Esc)")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ViewerEvent::Close))),
                    ),
            )
            .child(
                // Scrolls when zoomed past the window; clicking around the
                // image closes, like Desktop.
                div()
                    .id("image-viewer-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ViewerEvent::Close)))
                    .child(
                        div()
                            .min_w_full()
                            .min_h_full()
                            .p(px(MARGIN))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .id("image-viewer-image")
                                    .debug_selector(|| "viewed-image".into())
                                    .flex_none()
                                    .w(px(w))
                                    .h(px(h))
                                    .when(can_zoom, |d| d.cursor_pointer())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_zoom(cx);
                                    }))
                                    .child(img(self.image.path.clone()).size_full().object_fit(ObjectFit::Contain))
                                    .map(|d| {
                                        // Desktop offers no copy of a view-once photo.
                                        if self.image.view_once {
                                            return d.into_any_element();
                                        }
                                        let viewer = cx.weak_entity();
                                        d.context_menu(move |menu, _, _| {
                                            let viewer = viewer.clone();
                                            menu.item(PopupMenuItem::new("Copy Image").icon(IconName::Copy).on_click(
                                                move |_, _, cx| {
                                                    let _ = viewer.update(cx, |this, cx| this.copy(cx));
                                                },
                                            ))
                                        })
                                        .into_any_element()
                                    }),
                            ),
                    ),
            )
            // Desktop's previous and next, over the sides.
            .children([(-1, "lightbox-previous", IconName::ChevronLeft), (1, "lightbox-next", IconName::ChevronRight)].map(
                |(by, id, icon)| {
                    self.can_step(by).then(|| {
                        div()
                            .absolute()
                            .top(px(TOOLBAR_HEIGHT))
                            .bottom_0()
                            .w(px(80.))
                            .when(by < 0, |d| d.left_0())
                            .when(by > 0, |d| d.right_0())
                            .child(lightbox::nav_button(id, icon, by < 0).on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.step(by, cx);
                            })))
                    })
                },
            ).into_iter().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_names_are_safe_and_unique() {
        let dir = tempfile::tempdir().unwrap();
        let image = |name: Option<&str>| ViewedImage {
            path: PathBuf::from("/x/123-0.png"),
            size: None,
            file_name: name.map(Into::into),
            content_type: "image/png".into(),
            view_once: false,
            in_chat: None,
        };
        assert_eq!(download_name(&image(Some("../../etc/passwd"))), "etcpasswd");
        assert!(download_name(&image(None)).starts_with("signal-") && download_name(&image(None)).ends_with(".png"));

        std::fs::write(dir.path().join("cat.png"), b"").unwrap();
        std::fs::write(dir.path().join("cat (1).png"), b"").unwrap();
        assert_eq!(unique_path(dir.path(), "cat.png"), dir.path().join("cat (2).png"));
        assert_eq!(unique_path(dir.path(), "dog.png"), dir.path().join("dog.png"));
    }

    #[gpui_kit::test]
    fn opens_fitted_and_zooms_to_actual_size(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, VisualTestContext, WindowBounds, WindowOptions, size};
        cx.update(gpui_kit::init);
        let image = ViewedImage { path: PathBuf::from("/nonexistent.png"), size: Some((4000, 3000)), file_name: None, content_type: "image/png".into(), view_once: false, in_chat: None };
        let (window, viewer) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(1000.), px(700.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |_, cx| cx.new(|cx| ImageViewer::new(image, None, cx))).unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        let shown = |cx: &mut VisualTestContext| {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            cx.debug_bounds("viewed-image").expect("image laid out").size
        };
        // Fitted inside the window, aspect kept.
        let fitted = shown(&mut cx);
        assert!(fitted.width < px(1000.) && fitted.height < px(700.), "{fitted:?}");
        assert!((f32::from(fitted.width) / f32::from(fitted.height) - 4. / 3.).abs() < 0.01);

        viewer.update(&mut cx, |v, cx| v.actual_size(cx));
        assert_eq!(shown(&mut cx).width, px(4000.));
        // Zooming out from 100% steps down, and stops at fit.
        for _ in 0..20 {
            cx.update(|window, cx| viewer.update(cx, |v, cx| v.zoom_by(1. / ZOOM_STEP, available_area(window), cx)));
        }
        assert_eq!(shown(&mut cx), fitted);
    }

    #[gpui_kit::test]
    fn copies_the_file_as_its_stored_type(cx: &mut gpui_kit::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo");
        // Not a real JPEG: the stored type decides, not the bytes.
        std::fs::write(&path, b"bytes").unwrap();
        cx.update(|cx| copy_image(path.clone(), "image/jpeg", cx));
        cx.run_until_parked();
        let copied = cx.update(|cx| cx.read_from_clipboard()).and_then(|item| match item.entries().first()? {
            gpui_kit::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        });
        assert_eq!(copied, Some(Image::from_bytes(ImageFormat::Jpeg, b"bytes".to_vec())));

        // A type the clipboard can't carry leaves it as it was.
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("before".into())));
        cx.update(|cx| copy_image(path.clone(), "image/x-unknown", cx));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.read_from_clipboard()).and_then(|item| item.text()), Some("before".into()));
    }

    #[test]
    fn fit_never_enlarges() {
        // Limited by height.
        assert_eq!(fit_scale(Some((4000, 3000)), (1000., 600.)), 0.2);
        // A small image shows at 100%, not blown up.
        assert_eq!(fit_scale(Some((200, 100)), (1000., 600.)), 1.);
        assert_eq!(fit_scale(None, (1000., 600.)), 1.);
    }

    #[gpui_kit::test]
    fn a_view_once_copy_goes_when_the_viewer_closes(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, VisualTestContext};
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.png");
        std::fs::write(&path, b"").unwrap();
        let image = ViewedImage { path: path.clone(), size: Some((10, 10)), file_name: None, content_type: "image/png".into(), view_once: true, in_chat: None };
        let (window, viewer) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|cx| ImageViewer::new(image, None, cx))).unwrap()
        });
        // Only the window holds it now.
        drop(viewer);
        let mut vcx = VisualTestContext::from_window(window, cx);
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("viewed-image").is_some());
        assert!(path.exists());
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();
        assert!(!path.exists(), "deleted with the viewer");
    }
}
