//! Message composer with `:shortcode` emoji type-ahead and the emoji picker.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::component::input::{
    Enter, Escape, IndentInline, InputEvent, MoveDown, MoveUp, Paste, Textarea, TextareaState,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, ClipboardEntry, Context, Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable, FontWeight, Image,
    ImageFormat, ObjectFit, PathPromptOptions, SharedString, Subscription, Task, Window, deferred, div, img, px,
};
use signal_backend::{Command, LinkPreviewDraft, OutgoingFile, OutgoingImage};
use signal_emoji::{Emoji, SearchOptions};
use signal_store::{ConversationId, ConversationKind, Message};

use crate::state::AppState;
use crate::theme::Palette;
use crate::views::common::ClosedByClick;
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};

/// Rows shown in the type-ahead popup.
const TYPEAHEAD_LIMIT: usize = 8;
/// Re-send "started typing" this often while typing (Signal: every 10s).
const TYPING_REFRESH: Duration = Duration::from_secs(10);
const DRAFT_DEBOUNCE: Duration = Duration::from_millis(400);

pub enum ComposerEvent {
    Sent,
    /// ↑ in an empty composer: edit your last message (as Desktop).
    EditLast,
}

/// Signal Desktop waits this long after typing before fetching a preview.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(200);

/// The link preview for the draft's first link.
struct StagedPreview {
    url: String,
    state: PreviewState,
}

enum PreviewState {
    Loading,
    Ready { preview: std::rc::Rc<LinkPreviewDraft>, image: Option<Arc<Image>> },
    /// The page has no preview; nothing to show.
    Unavailable,
}

impl StagedPreview {
    fn release(&self, cx: &mut App) {
        if let PreviewState::Ready { image: Some(image), .. } = &self.state {
            gpui_kit::ImageSource::Image(image.clone()).remove_asset(cx);
        }
    }
}

/// A file (not an image) waiting to be sent, as a message of its own.
struct PendingFile {
    bytes: Arc<Vec<u8>>,
    file_name: String,
}

/// An image waiting to be sent with the next message.
struct PendingImage {
    bytes: Arc<Vec<u8>>,
    file_name: Option<String>,
    preview: Arc<Image>,
}

impl PendingImage {
    /// In-memory previews live in gpui's app-wide asset cache until removed.
    fn release(&self, cx: &mut App) {
        gpui_kit::ImageSource::Image(self.preview.clone()).remove_asset(cx);
    }
}

/// gpui's format for a preview, from the file's signature.
fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    Some(match bytes {
        [0x89, b'P', b'N', b'G', ..] => ImageFormat::Png,
        [0xFF, 0xD8, 0xFF, ..] => ImageFormat::Jpeg,
        [b'G', b'I', b'F', b'8', ..] => ImageFormat::Gif,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => ImageFormat::Webp,
        [b'I', b'I', b'*', 0, ..] | [b'M', b'M', 0, b'*', ..] => ImageFormat::Tiff,
        [b'B', b'M', ..] => ImageFormat::Bmp,
        _ => return None,
    })
}

struct Typeahead {
    range: Range<usize>,
    query: String,
    results: Suggestions,
    selected: usize,
}

/// What the type-ahead offers: emoji for `:sho`, group members for `@al`.
#[derive(Clone)]
enum Suggestions {
    Emoji(Vec<&'static Emoji>),
    /// `(service id, name)`.
    People(Vec<(String, String)>),
}

impl Suggestions {
    fn len(&self) -> usize {
        match self {
            Suggestions::Emoji(e) => e.len(),
            Suggestions::People(p) => p.len(),
        }
    }
}

/// Members offered at once (Desktop's list scrolls; eight fit).
const MENTION_LIMIT: usize = 8;

pub struct Composer {
    state: Entity<AppState>,
    conversation: ConversationId,
    pub(crate) input: Entity<TextareaState>,
    picker: Entity<EmojiPicker>,
    picker_open: bool,
    picker_closed_by: ClosedByClick,
    typeahead: Option<Typeahead>,
    reply_to: Option<Message>,
    /// The sent message being edited ("Edit message").
    pub(crate) editing: Option<Message>,
    /// A group's members, for @mentions: `(service id, name)`.
    members: Vec<(String, String)>,
    /// Mentions picked in this draft: `("@Name", service id)`.
    mentions: Vec<(String, String)>,
    images: Vec<PendingImage>,
    files: Vec<PendingFile>,
    link_preview: Option<StagedPreview>,
    /// Links whose preview was dismissed in this draft.
    dismissed_previews: std::collections::HashSet<String>,
    preview_task: Option<Task<()>>,
    typing_sent_at: Option<Instant>,
    draft_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(
        state: Entity<AppState>,
        picker: Entity<EmojiPicker>,
        conversation: ConversationId,
        draft: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx)
                .auto_grow(1, 10)
                .submit_on_enter(true)
                .placeholder("Message");
            if let Some(draft) = draft.filter(|d| !d.is_empty()) {
                input.set_value(draft, window, cx);
            }
            input
        });
        let subscriptions = vec![
            // Previews staged in a chat you switch away from.
            cx.on_release(|this, cx| {
                for image in &this.images {
                    image.release(cx);
                }
                if let Some(preview) = &this.link_preview {
                    preview.release(cx);
                }
            }),
            cx.subscribe(&state, |this, _, event: &crate::state::AppEvent, cx| {
                if let crate::state::AppEvent::LinkPreview { url, preview } = event {
                    this.on_link_preview(url, preview.clone(), cx);
                }
            }),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.on_change(window, cx),
                InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                InputEvent::Blur => this.stop_typing(cx),
                _ => {}
            }),
            cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, window, cx| {
                if !this.picker_open {
                    return;
                }
                match event {
                    PickerEvent::Picked(glyph) => {
                        this.insert_emoji(glyph.clone(), None, window, cx);
                        this.close_picker(window, cx);
                    }
                    PickerEvent::Sticker(pack, sticker) => {
                        let conversation = this.conversation.clone();
                        let (pack, sticker) = (pack.clone(), sticker.clone());
                        this.state.update(cx, |state, cx| state.send_sticker(conversation, pack, sticker, cx));
                        this.close_picker(window, cx);
                        cx.emit(ComposerEvent::Sent);
                    }
                    PickerEvent::Gif(gif) => {
                        let conversation = this.conversation.clone();
                        this.state.read(cx).send(Command::SendGif { conversation, gif: gif.clone() });
                        this.close_picker(window, cx);
                        cx.emit(ComposerEvent::Sent);
                    }
                    PickerEvent::Dismissed => this.close_picker(window, cx),
                }
            }),
        ];
        let mut this = Self {
            state,
            conversation,
            input,
            picker,
            picker_open: false,
            picker_closed_by: ClosedByClick::default(),
            typeahead: None,
            reply_to: None,
            editing: None,
            members: Vec::new(),
            mentions: Vec::new(),
            images: Vec::new(),
            files: Vec::new(),
            link_preview: None,
            dismissed_previews: Default::default(),
            preview_task: None,
            typing_sent_at: None,
            draft_task: None,
            _subscriptions: subscriptions,
        };
        this.load_members(cx);
        this
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let input = self.input.clone();
        input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Loads a group's member names for @mentions.
    fn load_members(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let Some(chat) = state.conversation(&self.conversation).filter(|c| c.kind == ConversationKind::Group) else {
            return;
        };
        let me = state.account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
        let members: Vec<String> = chat.members.iter().filter(|m| **m != me).cloned().collect();
        let backend = state.backend.clone();
        let db = backend.db().clone();
        let query = backend.spawn(async move { db.display_names(&members).await });
        cx.spawn(async move |this, cx| {
            let mut names = query.await.unwrap_or_default();
            names.sort_by_key(|(_, name)| name.to_lowercase());
            let _ = this.update(cx, |this, _| this.members = names);
        })
        .detach();
    }

    /// Desktop's "Edit": the message's text in the input, sent as an edit.
    pub fn start_edit(&mut self, message: Message, window: &mut Window, cx: &mut Context<Self>) {
        let body = message.body.clone();
        self.mentions = message
            .mentions
            .iter()
            .filter_map(|m| Some((body.get(m.start..m.start + m.len)?.to_owned(), m.aci.clone())))
            .collect();
        self.reply_to = None;
        self.editing = Some(message);
        self.input.update(cx, |input, cx| input.set_value(body, window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.mentions.clear();
            self.input.update(cx, |input, cx| input.set_value("", window, cx));
            cx.notify();
        }
    }

    pub fn reply_to(&mut self, message: Message, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_to = Some(message);
        self.focus(window, cx);
        cx.notify();
    }

    fn on_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, cursor) = {
            let input = self.input.read(cx);
            (input.value().to_string(), input.cursor())
        };

        // `:joy:` → 😂 as soon as the closing colon is typed.
        if let Some((range, emoji)) = signal_emoji::shortcode_replacement_at(&text, cursor) {
            let glyph = emoji.glyph_with(self.skin_tone(cx)).to_owned();
            self.insert_emoji(glyph, Some(range), window, cx);
            return;
        }

        // Nothing left to mention: forget picks, so "@Name" typed later is text.
        if text.is_empty() {
            self.mentions.clear();
        }
        self.update_typeahead(&text, cursor, cx);
        self.update_link_preview(&text, cx);
        self.update_typing(!text.trim().is_empty(), cx);
        self.save_draft_later(text, cx);
        cx.notify();
    }

    fn skin_tone(&self, cx: &App) -> Option<signal_emoji::SkinTone> {
        self.picker.read(cx).skin_tone()
    }

    fn update_typeahead(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) {
        if let Some((range, query)) = mention_query(text, cursor) {
            let needle = query.to_lowercase();
            let people: Vec<(String, String)> = self
                .members
                .iter()
                .filter(|(_, name)| {
                    let name = name.to_lowercase();
                    needle.is_empty() || name.starts_with(&needle) || name.split_whitespace().any(|w| w.starts_with(&needle))
                })
                .take(MENTION_LIMIT)
                .cloned()
                .collect();
            if !people.is_empty() {
                let selected = self.typeahead.as_ref().map_or(0, |t| t.selected.min(people.len() - 1));
                self.typeahead = Some(Typeahead { range, query, results: Suggestions::People(people), selected });
                return;
            }
        }
        let Some(completion) = signal_emoji::completion_at(text, cursor) else {
            self.typeahead = None;
            return;
        };
        let usage = &self.state.read(cx).emoji_usage;
        let options = SearchOptions {
            limit: TYPEAHEAD_LIMIT,
            max_version: crate::platform::max_emoji_version(),
            fuzzy: true,
        };
        let results: Vec<&'static Emoji> =
            signal_emoji::search_with_boost(&completion.query, &options, |id| usage.boost(id))
                .into_iter()
                .map(|m| m.emoji)
                .collect();
        if results.is_empty() {
            self.typeahead = None;
            return;
        }
        // Keep the highlighted row when refining the same query.
        let selected = match &self.typeahead {
            Some(Typeahead { results: Suggestions::Emoji(previous), query, selected, .. })
                if completion.query.starts_with(query.as_str()) =>
            {
                previous.get(*selected).and_then(|e| results.iter().position(|r| r == e)).unwrap_or(0)
            }
            _ => 0,
        };
        self.typeahead = Some(Typeahead {
            range: completion.range,
            query: completion.query,
            results: Suggestions::Emoji(results),
            selected,
        });
    }

    fn accept_typeahead(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(typeahead) = self.typeahead.take() else { return };
        let ix = index.unwrap_or(typeahead.selected);
        match &typeahead.results {
            Suggestions::Emoji(results) => {
                let Some(emoji) = results.get(ix) else { return };
                let glyph = emoji.glyph_with(self.skin_tone(cx)).to_owned();
                self.insert_emoji(glyph, Some(typeahead.range), window, cx);
            }
            Suggestions::People(people) => {
                let Some((aci, name)) = people.get(ix) else { return };
                let display = format!("@{name}");
                self.mentions.push((display.clone(), aci.clone()));
                self.input.update(cx, |input, cx| {
                    input.set_selected_range(typeahead.range.clone(), cx);
                    input.replace(format!("{display} "), window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            }
        }
    }

    /// Inserts `glyph` replacing `range` (or at the cursor) and records usage.
    fn insert_emoji(&mut self, glyph: String, range: Option<Range<usize>>, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            if let Some(range) = range {
                input.set_selected_range(range, cx);
                input.replace(glyph.clone(), window, cx);
            } else {
                input.insert(glyph.clone(), window, cx);
            }
            input.focus(window, cx);
        });
        self.typeahead = None;
        self.state.update(cx, |state, _| state.record_emoji(&glyph));
        let text = self.input.read(cx).value().to_string();
        self.update_typing(!text.trim().is_empty(), cx);
        self.save_draft_later(text, cx);
        cx.notify();
    }

    fn update_typing(&mut self, has_text: bool, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        if has_text {
            if self.typing_sent_at.is_none_or(|t| t.elapsed() > TYPING_REFRESH) {
                state.send(Command::Typing {
                    conversation: self.conversation.clone(),
                    started: true,
                });
                self.typing_sent_at = Some(Instant::now());
            }
        } else {
            self.stop_typing(cx);
        }
    }

    fn stop_typing(&mut self, cx: &mut Context<Self>) {
        if self.typing_sent_at.take().is_some() {
            self.state.read(cx).send(Command::Typing {
                conversation: self.conversation.clone(),
                started: false,
            });
        }
    }

    fn save_draft_later(&mut self, text: String, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let conversation = self.conversation.clone();
        self.draft_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DRAFT_DEBOUNCE).await;
            let db = backend.db().clone();
            let draft = (!text.trim().is_empty()).then_some(text);
            let _ = backend
                .spawn(async move { db.set_draft(&conversation, draft.as_deref()).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state.update(cx, |state, cx| state.reload_conversations(cx));
            });
        }));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.typeahead.is_some() {
            self.accept_typeahead(None, window, cx);
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        let (body, mentions) = encode_mentions(&text, &std::mem::take(&mut self.mentions));
        if let Some(original) = self.editing.take() {
            // Who's mentioned can change without the text changing (two people
            // with one name), so compare both.
            let mut before: Vec<&signal_store::MentionSpan> = original.mentions.iter().collect();
            before.sort_by_key(|m| m.start);
            let same_people = before.iter().map(|m| m.aci.as_str()).eq(mentions.iter().map(String::as_str));
            if !text.is_empty() && (text != original.body || !same_people) {
                self.state.read(cx).send(Command::EditMessage { message_id: original.id, body, mentions });
            }
            self.input.update(cx, |input, cx| input.set_value("", window, cx));
            self.save_draft_later(String::new(), cx);
            cx.notify();
            return;
        }
        if text.is_empty() && self.images.is_empty() && self.files.is_empty() {
            return;
        }
        let quote = self.reply_to.take().map(|m| m.id);
        let preview = match self.link_preview.take() {
            Some(staged) => {
                staged.release(cx);
                match staged.state {
                    PreviewState::Ready { preview, .. } => Some((*preview).clone()),
                    _ => None,
                }
            }
            None => None,
        };
        self.dismissed_previews.clear();
        self.preview_task = None;
        // The text goes with the images, else with the first file (each file
        // is a message of its own, as in Desktop).
        let files = std::mem::take(&mut self.files);
        let mut commands = Vec::new();
        let mut caption = Some((body, quote, mentions));
        if !self.images.is_empty() || files.is_empty() {
            let (body, quote, mentions) = caption.take().unwrap_or_default();
            commands.push(if self.images.is_empty() {
                Command::SendText { conversation: self.conversation.clone(), body, quote, preview, mentions }
            } else {
                let images = std::mem::take(&mut self.images)
                    .into_iter()
                    .map(|i| {
                        i.release(cx);
                        OutgoingImage { bytes: i.bytes, file_name: i.file_name }
                    })
                    .collect();
                Command::SendImages { conversation: self.conversation.clone(), images, caption: body, quote, mentions }
            });
        }
        for file in files {
            let (caption, quote, mentions) = caption.take().unwrap_or_default();
            commands.push(Command::SendFile {
                conversation: self.conversation.clone(),
                file: OutgoingFile { bytes: file.bytes, file_name: file.file_name },
                caption,
                quote,
                mentions,
            });
        }
        for command in commands {
            self.state.read(cx).send(command);
        }
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        self.typing_sent_at = None; // sending a message implicitly stops typing
        self.save_draft_later(String::new(), cx);
        cx.emit(ComposerEvent::Sent);
        cx.notify();
    }

    /// Keeps the staged preview in step with the draft's first link.
    fn update_link_preview(&mut self, text: &str, cx: &mut Context<Self>) {
        let link = signal_backend::link_preview::first_previewable(text).filter(|l| !self.dismissed_previews.contains(l));
        if self.link_preview.as_ref().map(|p| &p.url) == link.as_ref() {
            return;
        }
        if let Some(old) = self.link_preview.take() {
            old.release(cx);
        }
        self.preview_task = None;
        let Some(url) = link else { return };
        self.link_preview = Some(StagedPreview { url: url.clone(), state: PreviewState::Loading });
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.link_preview.as_ref().is_some_and(|p| p.url == url) {
                    this.state.read(cx).send(Command::FetchLinkPreview { url });
                }
            });
        }));
    }

    fn on_link_preview(&mut self, url: &str, preview: Option<std::rc::Rc<LinkPreviewDraft>>, cx: &mut Context<Self>) {
        let Some(staged) = self.link_preview.as_mut().filter(|p| p.url == url) else { return };
        staged.state = match preview {
            Some(preview) => {
                let image = preview.image.as_ref().and_then(|i| {
                    Some(Arc::new(Image::from_bytes(image_format(&i.bytes)?, i.bytes.as_ref().clone())))
                });
                PreviewState::Ready { preview, image }
            }
            None => PreviewState::Unavailable,
        };
        cx.notify();
    }

    fn render_link_preview(&self, palette: Palette, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let staged = self.link_preview.as_ref()?;
        let (title, domain, image) = match &staged.state {
            PreviewState::Unavailable => return None,
            PreviewState::Loading => ("Loading preview…".to_owned(), None, None),
            PreviewState::Ready { preview, image } => {
                let domain = url::Url::parse(&preview.url).ok().and_then(|u| u.host_str().map(str::to_owned));
                (preview.title.clone(), domain, image.clone())
            }
        };
        Some(
            div()
                .mx_3()
                .mt_2()
                .flex()
                .items_center()
                .gap_3()
                .p_2()
                .rounded_lg()
                .bg(palette.input_bg)
                .when_some(image, |d, image| {
                    d.child(img(image).size(px(48.)).flex_none().rounded_md().object_fit(ObjectFit::Cover))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                        .when_some(domain, |d, domain| {
                            d.child(div().text_xs().text_color(palette.text_secondary).truncate().child(domain))
                        }),
                )
                .child(
                    div()
                        .id("dismiss-link-preview")
                        .p_1()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(staged) = this.link_preview.take() {
                                staged.release(cx);
                                this.dismissed_previews.insert(staged.url);
                            }
                            this.preview_task = None;
                            cx.notify();
                        }))
                        .child(Icon::new(IconName::X).size_4().text_color(palette.text_secondary)),
                ),
        )
    }

    /// Queues an image to send; `false` if it isn't one.
    fn add_image(&mut self, bytes: Vec<u8>, file_name: Option<String>, cx: &mut Context<Self>) -> bool {
        let Some(format) = image_format(&bytes) else { return false };
        let preview = Arc::new(Image::from_bytes(format, bytes.clone()));
        self.images.push(PendingImage { bytes: Arc::new(bytes), file_name, preview });
        cx.notify();
        true
    }

    /// Queues a file to send as it is; `false` (with a message) when it's
    /// over Signal's limit.
    fn add_file(&mut self, bytes: Vec<u8>, file_name: Option<String>, cx: &mut Context<Self>) -> bool {
        if bytes.len() > signal_backend::MAX_ATTACHMENT_BYTES {
            return false;
        }
        self.files.push(PendingFile { bytes: Arc::new(bytes), file_name: file_name.unwrap_or_else(|| "file".into()) });
        cx.notify();
        true
    }

    /// Reads files off the UI thread and queues them: images to send as
    /// photos, anything else as a file.
    fn add_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let read = cx.background_spawn(async move {
            paths
                .into_iter()
                .map(|path| {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    std::fs::read(&path).ok().map(|bytes| (bytes, name))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let files = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let unreadable = files.iter().filter(|f| f.is_none()).count();
                let too_big = files
                    .into_iter()
                    .flatten()
                    .filter(|(bytes, name)| {
                        !this.add_image(bytes.clone(), name.clone(), cx) && !this.add_file(bytes.clone(), name.clone(), cx)
                    })
                    .count();
                let mut problems = Vec::new();
                if too_big > 0 {
                    problems.push(format!("{too_big} over Signal's 100 MB limit"));
                }
                if unreadable > 0 {
                    problems.push(format!("{unreadable} that couldn't be read"));
                }
                if !problems.is_empty() {
                    this.state.update(cx, |state, cx| {
                        state.report_error(format!("Skipped {} file(s): {}", too_big + unreadable, problems.join(", ")), cx);
                    });
                }
                this.focus(window, cx);
            });
        })
        .detach();
    }

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx));
            }
        })
        .detach();
    }

    /// ⌘V with an image (or copied image files) on the clipboard attaches
    /// it; plain text pastes as usual.
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let mut handled = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    handled |= self.add_image(image.bytes.clone(), None, cx);
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.add_paths(paths.paths().to_vec(), window, cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        handled
    }

    fn render_files(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_wrap().gap_2().mx_3().mt_2().children(self.files.iter().enumerate().map(|(ix, file)| {
            div()
                .id(("pending-file", ix))
                .flex()
                .items_center()
                .gap_2()
                .max_w(px(260.))
                .pl_2()
                .pr_1()
                .py_1()
                .rounded_lg()
                .bg(palette.input_bg)
                .border_1()
                .border_color(palette.panel_border)
                .child(Icon::new(IconName::File).size_4().text_color(palette.text_secondary))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(file.file_name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(palette.text_secondary)
                                .child(crate::views::common::format_size(file.bytes.len() as u64)),
                        ),
                )
                .child(
                    div()
                        .id(("remove-file", ix))
                        .flex_none()
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|d| d.bg(palette.panel_border))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if ix < this.files.len() {
                                this.files.remove(ix);
                            }
                            cx.notify();
                        }))
                        .child(Icon::new(IconName::X).size_3().text_color(palette.text)),
                )
        }))
    }

    fn render_images(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_wrap().gap_2().mx_3().mt_2().children(self.images.iter().enumerate().map(|(ix, image)| {
            div()
                .relative()
                .size(px(72.))
                .child(img(image.preview.clone()).size_full().rounded_lg().object_fit(ObjectFit::Cover))
                .child(
                    div()
                        .id(("remove-image", ix))
                        .absolute()
                        .top(px(-6.))
                        .right(px(-6.))
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(palette.panel_border)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if ix < this.images.len() {
                                this.images.remove(ix).release(cx);
                            }
                            cx.notify();
                        }))
                        .child(Icon::new(IconName::X).size_3().text_color(palette.text)),
                )
        }))
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker_open {
            self.close_picker(window, cx);
        } else {
            self.picker_open = true;
            self.typeahead = None;
            self.picker.update(cx, |picker, cx| {
                picker.reset(window, cx);
                picker.set_media(true, window, cx);
            });
            cx.notify();
        }
    }

    pub fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = false;
        self.focus(window, cx);
        cx.notify();
    }

    fn render_typeahead(&self, typeahead: &Typeahead, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tone = self.skin_tone(cx);
        div()
            .absolute()
            .bottom_full()
            .left_3()
            .mb_1()
            .w(px(320.))
            .py_1()
            .rounded_lg()
            .border_1()
            .border_color(palette.panel_border)
            .bg(palette.popover)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.typeahead.take().is_some() {
                    cx.notify();
                }
            }))
            .children((0..typeahead.results.len()).map(|ix| {
                let selected = ix == typeahead.selected;
                let row = div()
                    .id(("typeahead", ix))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .h(px(34.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(palette.row_selected))
                    .hover(|d| d.bg(palette.row_hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.accept_typeahead(Some(ix), window, cx)));
                match &typeahead.results {
                    Suggestions::Emoji(results) => {
                        let emoji = results[ix];
                        row.child(div().text_size(px(20.)).child(emoji.glyph_with(tone))).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(format!(":{}:", emoji.shortcode().unwrap_or(emoji.name))),
                        )
                    }
                    Suggestions::People(people) => {
                        let (aci, name) = &people[ix];
                        let photo = self.state.read(cx).photo_of(aci);
                        row.child(crate::views::common::person_avatar(photo, name, crate::theme::color_for(aci), px(24.)))
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(name.clone()))
                    }
                }
            }))
    }

    /// Desktop's "Edit message" banner over the input.
    fn render_editing(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .mx_3()
            .mt_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_lg()
            .bg(palette.input_bg)
            .child(div().w(px(4.)).self_stretch().rounded_l_lg().bg(palette.accent))
            .child(Icon::new(IconName::Pencil).size_4().text_color(palette.text_secondary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .py_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Edit message"))
                    .child(div().text_sm().truncate().text_color(palette.text_secondary).child(message.body.clone())),
            )
            .child(
                div()
                    .id("cancel-edit")
                    .p_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx)))
                    .child(Icon::new(IconName::X).size_4().text_color(palette.text_secondary)),
            )
    }

    fn render_reply(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let author: SharedString = match &message.author_name {
            Some(name) if message.author.is_some() => name.clone().into(),
            _ => "You".into(),
        };
        div()
            .mx_3()
            .mt_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_lg()
            .bg(palette.input_bg)
            .child(div().w(px(4.)).self_stretch().rounded_l_lg().bg(palette.accent))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .py_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(author))
                    .child(div().text_sm().truncate().text_color(palette.text_secondary).child(message.body.clone())),
            )
            .child(
                div()
                    .id("cancel-reply")
                    .p_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reply_to = None;
                        cx.notify();
                    }))
                    .child(Icon::new(IconName::X).size_4().text_color(palette.text_secondary)),
            )
    }
}

/// An `@query` being typed at `cursor`: the `@` must start the text or
/// follow a space, and the query has no spaces yet.
fn mention_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    if query.chars().any(char::is_whitespace) || query.chars().count() > 30 {
        return None;
    }
    if !before[..at].is_empty() && !before[..at].ends_with(char::is_whitespace) {
        return None;
    }
    Some((at..cursor, query.to_owned()))
}

/// Turns the picked "@Name"s still in `text` into Signal's placeholders,
/// with who each one mentions, in order. A mention you've since edited
/// stays as plain text.
fn encode_mentions(text: &str, picked: &[(String, String)]) -> (String, Vec<String>) {
    let mut hits: Vec<(usize, usize, &str)> = Vec::new();
    for (display, aci) in picked {
        let mut from = 0;
        while let Some(pos) = text.get(from..).and_then(|rest| rest.find(display.as_str())) {
            let start = from + pos;
            let end = start + display.len();
            if hits.iter().all(|(s, l, _)| end <= *s || start >= s + l) {
                hits.push((start, display.len(), aci));
                break;
            }
            from = end;
        }
    }
    hits.sort_by_key(|(start, _, _)| *start);
    let mut body = String::with_capacity(text.len());
    let mut last = 0;
    for (start, len, _) in &hits {
        body.push_str(&text[last..*start]);
        body.push(signal_backend::mapping::MENTION_PLACEHOLDER);
        last = start + len;
    }
    body.push_str(&text[last..]);
    (body, hits.into_iter().map(|(_, _, aci)| aci.to_owned()).collect())
}

impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let typeahead_open = self.typeahead.is_some();
        let can_send =
            !self.input.read(cx).value().trim().is_empty() || !self.images.is_empty() || !self.files.is_empty();

        div()
            .relative()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(palette.panel_border)
            .bg(palette.background)
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                if this.paste_image(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_paths(paths.paths().to_vec(), window, cx);
            }))
            // Steal navigation keys from the textarea while the popup is open.
            .when(typeahead_open, |d| {
                d.capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                    if let Some(t) = &mut this.typeahead {
                        cx.stop_propagation();
                        t.selected = t.selected.checked_sub(1).unwrap_or(t.results.len() - 1);
                        cx.notify();
                    }
                }))
                .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                    if let Some(t) = &mut this.typeahead {
                        cx.stop_propagation();
                        t.selected = (t.selected + 1) % t.results.len();
                        cx.notify();
                    }
                }))
                .capture_action(cx.listener(|this, enter: &Enter, window, cx| {
                    if !enter.shift && this.typeahead.is_some() {
                        cx.stop_propagation();
                        this.accept_typeahead(None, window, cx);
                    }
                }))
                .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                    if this.typeahead.is_some() {
                        cx.stop_propagation();
                        this.accept_typeahead(None, window, cx);
                    }
                }))
                .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                    if this.typeahead.take().is_some() {
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
            })
            .when_some(self.typeahead.as_ref(), |d, t| d.child(deferred(self.render_typeahead(t, palette, cx))))
            .when(self.picker_open, |d| {
                d.child(deferred(
                    div()
                        .absolute()
                        .bottom_full()
                        .left_2()
                        .mb_1()
                        // Leaves focus to whatever was clicked, unlike close_picker.
                        .on_mouse_down_out(cx.listener(|this, event, _, cx| {
                            this.picker_closed_by.record(event);
                            this.picker_open = false;
                            cx.notify();
                        }))
                        .child(self.picker.clone()),
                ))
            })
            .when_some(self.reply_to.clone(), |d, m| d.child(self.render_reply(&m, palette, cx)))
            .when_some(self.editing.clone(), |d, m| d.child(self.render_editing(&m, palette, cx)))
            // ↑ in an empty composer edits your last message; Esc cancels an edit.
            .when(!typeahead_open, |d| {
                d.capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                    if this.editing.is_none()
                        && this.images.is_empty()
                        && this.files.is_empty()
                        && this.input.read(cx).value().is_empty()
                    {
                        cx.stop_propagation();
                        cx.emit(ComposerEvent::EditLast);
                    }
                }))
                .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                    if this.editing.is_some() {
                        cx.stop_propagation();
                        this.cancel_edit(window, cx);
                    }
                }))
            })
            .when_some(self.render_link_preview(palette, cx), |d, card| d.child(card))
            .when(!self.images.is_empty(), |d| d.child(self.render_images(palette, cx)))
            .when(!self.files.is_empty(), |d| d.child(self.render_files(palette, cx)))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .id("emoji-button")
                            .debug_selector(|| "emoji-button".into())
                            .flex_none()
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|d| d.bg(palette.row_hover))
                            .on_click(cx.listener(|this, click, window, cx| {
                                if !this.picker_closed_by.take(click) {
                                    this.toggle_picker(window, cx);
                                }
                            }))
                            .child(Icon::new(IconName::FaceSlightlySmiling).size_5().text_color(palette.text_secondary)),
                    )
                    .child(
                        div()
                            .id("attach-button")
                            .flex_none()
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|d| d.bg(palette.row_hover))
                            .on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx)))
                            .child(Icon::new(IconName::Paperclip).size_5().text_color(palette.text_secondary)),
                    )
                    .child(
                        // One line tall when empty, like Signal Desktop: the
                        // pill supplies the padding, the textarea none.
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(px(12.))
                            .py(px(6.))
                            .rounded(px(16.))
                            .bg(palette.input_bg)
                            .child(
                                Textarea::new(&self.input)
                                    .appearance(false)
                                    .xsmall()
                                    .text_size(px(14.5))
                                    .line_height(px(20.)),
                            ),
                    )
                    .when(can_send, |d| {
                        d.child(
                            div()
                                .id("send-button")
                                .flex_none()
                                .size(px(32.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(palette.accent)
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                                .child(Icon::new(IconName::Send).size_4().text_color(palette.on_accent)),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{ElementId, Entity, TestAppContext, WindowBounds, WindowOptions, size, px};
    use signal_backend::{Backend, Config};
    use signal_store::ConversationId;

    use super::*;

    #[test]
    fn at_starts_a_mention_after_a_space() {
        assert_eq!(mention_query("hi @al", 6), Some((3..6, "al".into())));
        assert_eq!(mention_query("@", 1), Some((0..1, String::new())));
        assert_eq!(mention_query("me@example", 10), None, "an email address");
        assert_eq!(mention_query("hi @al ice", 10), None, "a space ends it");
    }

    #[test]
    fn picked_mentions_become_placeholders_in_order() {
        let alice = ("@Alice Liddell".to_owned(), "a".to_owned());
        let bob = ("@Bob".to_owned(), "b".to_owned());
        let (body, acis) = encode_mentions("@Bob and @Alice Liddell, @Bob", &[alice.clone(), bob.clone(), bob.clone()]);
        assert_eq!(body, "\u{FFFC} and \u{FFFC}, \u{FFFC}");
        assert_eq!(acis, ["b", "a", "b"]);
        // Edited away: plain text.
        let (body, acis) = encode_mentions("@Ali", &[alice]);
        assert_eq!((body.as_str(), acis.len()), ("@Ali", 0));
    }

    struct Fixture {
        window: gpui_kit::AnyWindowHandle,
        composer: Entity<Composer>,
        backend: Backend,
        _dir: tempfile::TempDir,
    }

    impl Drop for Fixture {
        // Close the database before the test process can exit mid-write.
        fn drop(&mut self) {
            self.backend.shutdown();
        }
    }

    fn mount(cx: &mut TestAppContext) -> Fixture {
        cx.update(gpui_kit::init);
        // The demo backend answers from its own Tokio threads.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (backend, _events) = Backend::start(Config {
            data_dir: dir.path().to_owned(),
            db_key: [7; 32],
            device_name: "test".into(),
            demo: true,
        })
        .unwrap();
        let (window, composer) = cx.update(|cx| {
            let state = crate::state::new_state(backend.clone(), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(800.), px(600.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
                cx.new(|cx| Composer::new(state, picker, ConversationId("d:test".into()), None, window, cx))
            })
            .unwrap()
        });
        Fixture { window, composer, backend, _dir: dir }
    }

    fn input_id(fixture: &Fixture, cx: &mut TestAppContext) -> ElementId {
        let input = cx.update(|cx| fixture.composer.read(cx).input.clone());
        ("input", input.entity_id()).into()
    }

    fn text(fixture: &Fixture, cx: &mut TestAppContext) -> String {
        cx.update(|cx| fixture.composer.read(cx).input.read(cx).value().to_string())
    }

    #[gpui_kit::test]
    fn clicking_off_the_picker_closes_it(cx: &mut TestAppContext) {
        let f = mount(cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        cx.run_until_parked();
        let button = cx.debug_bounds("emoji-button").expect("emoji button drawn").center();
        let open = |cx: &mut gpui_kit::VisualTestContext| cx.update(|_, cx| f.composer.read(cx).picker_open);
        let click = |cx: &mut gpui_kit::VisualTestContext, at| {
            cx.simulate_click(at, gpui_kit::Modifiers::none());
            cx.run_until_parked();
        };
        click(&mut cx, button);
        assert!(open(&mut cx));
        // Its own button closes it rather than closing and reopening it.
        click(&mut cx, button);
        assert!(!open(&mut cx));
        click(&mut cx, button);
        assert!(open(&mut cx));
        click(&mut cx, gpui_kit::point(px(790.), px(590.)));
        assert!(!open(&mut cx), "a click off the picker closes it");
    }

    #[gpui_kit::test]
    fn typing_a_link_stages_its_preview(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("see https://signal.org/blog/ and http://plain.org", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let staged = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                f.composer.read(cx).link_preview.as_ref().map(|p| (p.url.clone(), matches!(p.state, PreviewState::Loading)))
            })
        };
        // Only the https link qualifies; loading until the backend answers.
        assert_eq!(staged(cx), Some(("https://signal.org/blog/".into(), true)));

        let draft = LinkPreviewDraft {
            url: "https://signal.org/blog/".into(),
            title: "Blog".into(),
            description: None,
            date: None,
            image: None,
        };
        f.composer.update(cx, |c, cx| c.on_link_preview("https://signal.org/blog/", Some(std::rc::Rc::new(draft)), cx));
        assert_eq!(staged(cx), Some(("https://signal.org/blog/".into(), false)));

        // A stale answer for another link is ignored; removing the link drops it.
        f.composer.update(cx, |c, cx| c.on_link_preview("https://other.org/", None, cx));
        assert_eq!(staged(cx), Some(("https://signal.org/blog/".into(), false)));
        cx.update_window(f.window, |_, window, cx| {
            // set_value doesn't emit a change like typing does.
            f.composer.update(cx, |c, cx| {
                c.input.update(cx, |i, cx| i.set_value("no link now", window, cx));
                c.on_change(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(staged(cx), None);
    }

    /// A 1×1 PNG.
    const PIXEL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00,
        0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF,
        0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[gpui_kit::test]
    fn pasting_an_image_attaches_it_and_text_still_pastes(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| window.click(target, cx)).unwrap();

        let image = gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, PIXEL_PNG.to_vec());
        cx.update(|cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_image(&image)));
        cx.update_window(f.window, |_, window, cx| window.dispatch_action(Box::new(Paste), cx)).unwrap();
        cx.run_until_parked();
        let (images, text_after_image) =
            cx.update(|cx| (f.composer.read(cx).images.len(), f.composer.read(cx).input.read(cx).value().to_string()));
        assert_eq!((images, text_after_image.as_str()), (1, ""));

        cx.update(|cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("caption".into())));
        cx.update_window(f.window, |_, window, cx| window.dispatch_action(Box::new(Paste), cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(text(&f, cx), "caption");
        assert_eq!(cx.update(|cx| f.composer.read(cx).images.len()), 1);

        // Sending takes the images and clears them.
        cx.update_window(f.window, |_, window, cx| {
            f.composer.update(cx, |composer, cx| composer.submit(window, cx));
        })
        .unwrap();
        assert_eq!(cx.update(|cx| f.composer.read(cx).images.len()), 0);
        assert_eq!(text(&f, cx), "");
    }

    /// The demo's messages in the composer's chat, once `filter` matches one.
    fn sent(f: &Fixture, filter: impl Fn(&signal_store::Message) -> bool) -> Vec<signal_store::Message> {
        for _ in 0..300 {
            let db = f.backend.db().clone();
            let messages: Vec<_> = futures::executor::block_on(
                f.backend.spawn(async move { db.messages(&ConversationId("d:test".into()), None, 50).await }),
            )
            .unwrap_or_default();
            if messages.iter().any(&filter) {
                return messages;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("nothing was sent");
    }

    #[gpui_kit::test]
    fn files_send_as_their_own_messages_with_the_caption(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("the notes", cx);
        })
        .unwrap();
        f.composer.update(cx, |c, cx| {
            assert!(c.add_file(b"%PDF-1.7".to_vec(), Some("notes.pdf".into()), cx));
            assert!(c.add_file(b"a,b".to_vec(), Some("data.csv".into()), cx));
            // Over Signal's limit: refused.
            assert!(!c.add_file(vec![0; signal_backend::MAX_ATTACHMENT_BYTES + 1], Some("big.bin".into()), cx));
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| f.composer.read(cx).files.len()), 2);
        cx.update_window(f.window, |_, window, cx| f.composer.update(cx, |c, cx| c.submit(window, cx))).unwrap();
        assert_eq!(cx.update(|cx| f.composer.read(cx).files.len()), 0);

        let messages = sent(&f, |m| m.attachments.iter().any(|a| a.file_name.as_deref() == Some("data.csv")));
        let file = |name: &str| messages.iter().find(|m| m.attachments[0].file_name.as_deref() == Some(name)).unwrap().clone();
        let (pdf, csv) = (file("notes.pdf"), file("data.csv"));
        // The text goes with the first file; each file is its own message.
        assert_eq!((pdf.body.as_str(), pdf.attachments[0].content_type.as_str()), ("the notes", "application/pdf"));
        assert_eq!((csv.body.as_str(), csv.attachments[0].content_type.as_str()), ("", "text/csv"));
        let saved = std::fs::read(pdf.attachments[0].local_path.as_ref().unwrap()).unwrap();
        assert_eq!(saved, b"%PDF-1.7");
    }

    #[gpui_kit::test]
    fn typeahead_opens_navigates_and_accepts(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("hi :sm", cx);
        })
        .unwrap();
        cx.run_until_parked();

        let (query, results) = cx.update(|cx| {
            let t = f.composer.read(cx).typeahead.as_ref().expect("typeahead open");
            let Suggestions::Emoji(results) = &t.results else { panic!("emoji expected") };
            (t.query.clone(), results.clone())
        });
        assert_eq!(query, "sm");
        assert!(results.len() > 1);

        cx.update_window(f.window, |_, window, cx| window.press("down", cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| f.composer.read(cx).typeahead.as_ref().unwrap().selected), 1);

        // Enter accepts the highlighted emoji instead of sending.
        cx.update_window(f.window, |_, window, cx| window.press("enter", cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(text(&f, cx), format!("hi {}", results[1].glyph));
        assert!(cx.update(|cx| f.composer.read(cx).typeahead.is_none()));
    }

    #[gpui_kit::test]
    fn escape_closes_typeahead_and_typing_refines(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input(":pizz", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let first = cx.update(|cx| match &f.composer.read(cx).typeahead.as_ref().unwrap().results {
            Suggestions::Emoji(results) => results[0].name,
            Suggestions::People(_) => panic!("emoji expected"),
        });
        assert_eq!(first, "pizza");

        cx.update_window(f.window, |_, window, cx| window.press("escape", cx)).unwrap();
        cx.run_until_parked();
        assert!(cx.update(|cx| f.composer.read(cx).typeahead.is_none()));
        assert_eq!(text(&f, cx), ":pizz");
    }

    #[gpui_kit::test]
    fn complete_shortcode_is_replaced(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("lol :joy:", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(text(&f, cx), "lol 😂");
    }

    #[gpui_kit::test]
    fn enter_sends_and_clears(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = sent.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&f.composer, move |_, event: &ComposerEvent, _| match event {
                ComposerEvent::Sent => counter.set(counter.get() + 1),
                ComposerEvent::EditLast => {}
            })
        });
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("hello", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(sent.get(), 1);
        assert_eq!(text(&f, cx), "");
    }
}
