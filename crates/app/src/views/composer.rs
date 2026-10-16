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
use signal_backend::{Command, LinkPreviewDraft, OutgoingFile, OutgoingImage, OutgoingViewOnce};
use signal_emoji::{Emoji, SearchOptions};
use signal_store::{ConversationId, ConversationKind, Message};

use crate::recorder::Recording;
use crate::state::AppState;
use crate::theme::Palette;
use crate::views::common::ClosedByClick;
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};

/// Rows shown in the type-ahead popup.
const TYPEAHEAD_LIMIT: usize = 8;
/// Re-send "started typing" this often while typing (Signal: every 10s).
const TYPING_REFRESH: Duration = Duration::from_secs(10);
const DRAFT_DEBOUNCE: Duration = Duration::from_millis(400);

// Desktop's formatting shortcuts (bound in main.rs): each wraps the
// selection in its marker (see `crate::formatting`).
gpui_kit::actions!(composer, [FormatBold, FormatItalic, FormatStrikethrough, FormatMonospace, FormatSpoiler]);
// Desktop's shortcut for recording a voice message (cmd-shift-y).
gpui_kit::actions!(composer, [RecordVoice]);

/// A stopped recording to play back: its waveform (once decoded), and its
/// player while it plays.
struct VoiceDraft {
    wave: Option<(Vec<f32>, f64)>,
    player: Option<crate::audio::Player>,
    _poll: Option<Task<()>>,
}

/// How often the draft's playback is redrawn.
const DRAFT_POLL: Duration = Duration::from_millis(50);

/// How often the recording's level and time are read.
const RECORDING_POLL: Duration = Duration::from_millis(100);
/// What a recorded voice message is called on disk here (it's sent without
/// a name).
const VOICE_FILE_NAME: &str = "Voice message.m4a";

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
    /// Desktop's view-once toggle: the one photo or video staged goes to
    /// be viewed once, while [`Self::view_once_eligible`] holds.
    view_once: bool,
    link_preview: Option<StagedPreview>,
    /// Links whose preview was dismissed in this draft.
    dismissed_previews: std::collections::HashSet<String>,
    preview_task: Option<Task<()>>,
    typing_sent_at: Option<Instant>,
    draft_task: Option<Task<()>>,
    /// A voice message being recorded; the composer shows it instead of
    /// the input.
    recording: Option<Recording>,
    /// The recording stopped and kept to play back before sending
    /// (Desktop's draft recording).
    voice_draft: Option<VoiceDraft>,
    recording_focus: FocusHandle,
    _recording_poll: Option<Task<()>>,
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
                // A recording left unsent when the chat closes is dropped.
                if let Some(recording) = this.recording.take() {
                    drop(this._recording_poll.take());
                    let path = recording.path.clone();
                    drop(recording);
                    let _ = std::fs::remove_file(path);
                }
            }),
            cx.subscribe(&state, |this, _, event: &crate::state::AppEvent, cx| {
                if let crate::state::AppEvent::LinkPreview { url, preview } = event {
                    this.on_link_preview(url, preview.clone(), cx);
                }
            }),
            // Desktop: leaving the app stops a recording and keeps it as a
            // draft (`saveDraftRecordingIfNeeded` on blur).
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.save_draft_recording(cx);
                }
            }),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.on_typed(window, cx),
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
                        // The sticker replies; the reply stays for a draft
                        // still to send, as in Desktop's `sendStickerMessage`
                        // (`ts/state/ducks/composer.preload.ts`). Without
                        // the `stickerReply.send` flag it goes on its own
                        // and the reply stays, as there.
                        let replies = signal_backend::remote_config().sticker_reply_send();
                        let quote = this.take_reply_for_media(replies, cx);
                        this.state.update(cx, |state, cx| state.send_sticker(conversation, pack, sticker, quote, cx));
                        this.close_picker(window, cx);
                        cx.emit(ComposerEvent::Sent);
                    }
                    PickerEvent::Gif(gif) => {
                        let conversation = this.conversation.clone();
                        // A GIF replies too, as Desktop's goes with the
                        // composer's quote (`sendMultiMediaMessage`). Desktop
                        // also takes the draft as its caption, so its reply
                        // always goes; here the draft stays in the composer,
                        // so the reply stays with it, as for a sticker.
                        let quote = this.take_reply_for_media(true, cx);
                        this.state.read(cx).send(Command::SendGif { conversation, gif: gif.clone(), quote });
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
            view_once: false,
            link_preview: None,
            dismissed_previews: Default::default(),
            preview_task: None,
            typing_sent_at: None,
            draft_task: None,
            recording: None,
            voice_draft: None,
            recording_focus: cx.focus_handle(),
            _recording_poll: None,
            _subscriptions: subscriptions,
        };
        this.load_members(cx);
        this
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        if self.recording.is_some() {
            return window.focus(&self.recording_focus, cx);
        }
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
        self.mentions = message
            .mentions
            .iter()
            .filter_map(|m| Some((message.body.get(m.start..m.start + m.len)?.to_owned(), m.aci.clone())))
            .collect();
        // Its formatting, as markers to edit.
        let body = crate::formatting::to_markers(&message.body, &message.styles);
        self.reply_to = None;
        self.editing = Some(message);
        self.input.update(cx, |input, cx| input.set_value(body, window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    /// Wraps (or unwraps) the selection in `style`'s marker.
    fn format(&mut self, style: signal_store::TextStyle, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = {
            let input = self.input.read(cx);
            (input.value().to_string(), input.selected_range())
        };
        let (text, selection) = crate::formatting::toggle(&text, selection, style);
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.set_selected_range(selection, cx);
        });
        // `set_value` reports no change: the draft, typing and send button
        // follow by hand.
        self.on_change(window, cx);
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

    /// The text changed by typing (not by `set_value`): Desktop converts
    /// emoticons as they're typed (`AutoSubstituteAsciiEmojis`, only for
    /// the user's own edits), when "Convert typed emoticons to emoji" is on.
    fn on_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).prefs.auto_convert_emoji {
            let (text, cursor) = {
                let input = self.input.read(cx);
                (input.value().to_string(), input.cursor())
            };
            if let Some((range, emoji)) = signal_emoji::emoticon_replacement_at(&text, cursor) {
                let glyph = emoji.glyph_with(self.skin_tone(cx)).to_owned();
                self.insert_emoji(glyph, Some(range), window, cx);
                return;
            }
        }
        self.on_change(window, cx);
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

    /// Desktop's microphone button: records until Send or Cancel (or Esc).
    /// A voice message goes alone, so not with images or files staged.
    fn start_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::notification::Notification;
        if self.recording.is_some() || self.editing.is_some() {
            return;
        }
        if !self.images.is_empty() || !self.files.is_empty() {
            window.push_notification(Notification::new().message("A voice message must have only one attachment."), cx);
            return;
        }
        let path = crate::recorder::new_path();
        let start = self.state.read(cx).start_recorder;
        let recorder = match start(&path) {
            Ok(recorder) => recorder,
            Err(error) => {
                tracing::warn!(%error, "couldn't start recording");
                let _ = std::fs::remove_file(&path);
                window.push_notification(Notification::error("There was an error with the voice recorder."), cx);
                return;
            }
        };
        // One sound at a time.
        self.state.read(cx).pause_audio();
        self.stop_typing(cx);
        self.recording = Some(Recording::new(path, recorder));
        window.push_notification(Notification::new().message("Voice messages are limited to one hour."), cx);
        window.focus(&self.recording_focus, cx);
        self._recording_poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(RECORDING_POLL).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    let Some(recording) = &mut this.recording else { return false };
                    if recording.tick() {
                        this.enter_draft(cx);
                        this.confirm_over_limit(window, cx);
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Desktop's dialog when an hour is up: the recording has stopped; send
    /// it or not.
    fn confirm_over_limit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::dialog::DialogFooter;
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (discard, send) = (this.clone(), this.clone());
            dialog
                .w(px(420.))
                .child(div().text_sm().child("Voice message recording stopped because the maximum time limit was reached."))
                .footer(
                    DialogFooter::new()
                        .child(Button::new("voice-discard").label("Discard").outline().on_click(move |_, window, cx| {
                            window.close_all_dialogs(cx);
                            let _ = discard.update(cx, |this, cx| this.cancel_recording(window, cx));
                        }))
                        .child(Button::new("voice-send-anyway").label("Send anyway").primary().on_click(
                            move |_, window, cx| {
                                window.close_all_dialogs(cx);
                                let _ = send.update(cx, |this, cx| this.send_recording(window, cx));
                            },
                        )),
                )
        });
    }

    fn end_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Recording> {
        use gpui_kit::component::WindowExt as _;
        let mut recording = self.recording.take()?;
        // Stopped at the limit: its dialog is open, and is answered now.
        if recording.stopped && recording.elapsed() >= crate::recorder::MAX_SECONDS {
            window.close_all_dialogs(cx);
        }
        // Stops a draft playing.
        self.voice_draft = None;
        recording.stop();
        self._recording_poll = None;
        self.focus(window, cx);
        cx.notify();
        Some(recording)
    }

    /// Stops recording and keeps it as a draft, if it's still recording.
    fn save_draft_recording(&mut self, cx: &mut Context<Self>) {
        let Some(recording) = self.recording.as_mut().filter(|r| !r.stopped) else { return };
        recording.stop();
        self._recording_poll = None;
        self.enter_draft(cx);
    }

    /// Shows the stopped recording as a draft, decoding its waveform.
    fn enter_draft(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.recording.as_ref().map(|r| r.path.clone()) else { return };
        self.voice_draft = Some(VoiceDraft { wave: None, player: None, _poll: None });
        cx.spawn(async move |this, cx| {
            let wave = cx.background_executor().spawn(async move { crate::audio::waveform(&path).ok() }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(draft) = &mut this.voice_draft {
                    draft.wave = wave;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Plays or pauses the draft (one sound at a time).
    fn toggle_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.recording.as_ref().map(|r| r.path.clone()) else { return };
        let Some(draft) = &mut self.voice_draft else { return };
        if draft.player.is_none() {
            match crate::audio::Player::open(&path) {
                Ok(player) => draft.player = Some(player),
                Err(error) => {
                    use gpui_kit::component::WindowExt as _;
                    tracing::warn!(error = %format!("{error:#}"), "couldn't play the draft");
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error("There was an error with the voice recorder."),
                        cx,
                    );
                    return;
                }
            }
        }
        let Some(player) = &draft.player else { return };
        if player.is_playing() {
            player.pause();
        } else {
            self.state.read(cx).pause_audio();
            // Played to the end: again from the start.
            if player.duration() > 0. && player.position() >= player.duration() - 0.05 {
                player.seek(0.);
            }
            player.play();
            draft._poll = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(DRAFT_POLL).await;
                    let playing = this.update(cx, |this, cx| {
                        cx.notify();
                        this.voice_draft.as_ref().and_then(|d| d.player.as_ref()).is_some_and(|p| p.is_playing())
                    });
                    if !matches!(playing, Ok(true)) {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// A click on the draft's waveform: plays from there.
    fn seek_draft(&mut self, fraction: f64, window: &mut Window, cx: &mut Context<Self>) {
        let playing = self.voice_draft.as_ref().and_then(|d| d.player.as_ref()).is_some_and(|p| p.is_playing());
        if !playing {
            self.toggle_draft(window, cx);
        }
        if let Some(player) = self.voice_draft.as_ref().and_then(|d| d.player.as_ref()) {
            player.seek(fraction * player.duration());
        }
        cx.notify();
    }

    fn cancel_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(recording) = self.end_recording(window, cx) {
            let _ = std::fs::remove_file(&recording.path);
        }
    }

    /// Sends the recording as a voice message (replying, if a reply was
    /// staged).
    fn send_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::notification::Notification;
        let Some(recording) = self.end_recording(window, cx) else { return };
        let bytes = std::fs::read(&recording.path);
        let _ = std::fs::remove_file(&recording.path);
        let bytes = match bytes {
            Ok(bytes) if !bytes.is_empty() => bytes,
            result => {
                tracing::warn!(error = ?result.err(), "the recording is empty or unreadable");
                window.push_notification(Notification::error("There was an error with the voice recorder."), cx);
                return;
            }
        };
        let quote = self.reply_to.take().map(|m| m.id);
        self.state.read(cx).send(Command::SendFile {
            conversation: self.conversation.clone(),
            file: OutgoingFile { bytes: Arc::new(bytes), file_name: VOICE_FILE_NAME.into(), voice: true },
            caption: String::new(),
            quote,
            mentions: Vec::new(),
            styles: Vec::new(),
        });
        cx.emit(ComposerEvent::Sent);
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

    /// The reply a sticker or GIF sends with: it stays for a draft still to
    /// send, as in Desktop's `sendStickerMessage`
    /// (`ts/state/ducks/composer.preload.ts`).
    fn take_reply_for_media(&mut self, replies: bool, cx: &App) -> Option<i64> {
        if !replies {
            return None;
        }
        let quote = self.reply_to.as_ref().map(|m| m.id);
        if self.input.read(cx).value().is_empty() && self.images.is_empty() && self.files.is_empty() {
            self.reply_to = None;
        }
        quote
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.typeahead.is_some() {
            self.accept_typeahead(None, window, cx);
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        // Desktop's composer stops at 64 KiB with this toast
        // (`ts/components/CompositionInput.dom.tsx`); here the text stays to
        // be shortened.
        if text.len() > signal_backend::long_text::MAX_TEXT_BYTES {
            use gpui_kit::component::WindowExt as _;
            use gpui_kit::component::notification::Notification;
            window.push_notification(Notification::new().message("Message body is too long."), cx);
            return;
        }
        let (plain, styles) = crate::formatting::parse(&text);
        let (body, mentions, styles) = encode_mentions(&plain, &std::mem::take(&mut self.mentions), &styles);
        if let Some(original) = self.editing.take() {
            // Who's mentioned can change without the text changing (two people
            // with one name), so compare both.
            let mut before: Vec<&signal_store::MentionSpan> = original.mentions.iter().collect();
            before.sort_by_key(|m| m.start);
            let same_people = before.iter().map(|m| m.aci.as_str()).eq(mentions.iter().map(String::as_str));
            let original_text = crate::formatting::to_markers(&original.body, &original.styles);
            if !text.is_empty() && (text != original_text || !same_people) {
                self.state.read(cx).send(Command::EditMessage { message_id: original.id, body, mentions, styles });
            }
            self.input.update(cx, |input, cx| input.set_value("", window, cx));
            self.save_draft_later(String::new(), cx);
            cx.notify();
            return;
        }
        if text.is_empty() && self.images.is_empty() && self.files.is_empty() {
            return;
        }
        if self.view_once && self.view_once_eligible() {
            return self.send_view_once(cx);
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
        let mut caption = Some((body, quote, mentions, styles));
        if !self.images.is_empty() || files.is_empty() {
            let (body, quote, mentions, styles) = caption.take().unwrap_or_default();
            commands.push(if self.images.is_empty() {
                Command::SendText { conversation: self.conversation.clone(), body, quote, preview, mentions, styles }
            } else {
                let images = std::mem::take(&mut self.images)
                    .into_iter()
                    .map(|i| {
                        i.release(cx);
                        OutgoingImage { bytes: i.bytes, file_name: i.file_name }
                    })
                    .collect();
                Command::SendImages { conversation: self.conversation.clone(), images, caption: body, quote, mentions, styles }
            });
        }
        for file in files {
            let (caption, quote, mentions, styles) = caption.take().unwrap_or_default();
            commands.push(Command::SendFile {
                conversation: self.conversation.clone(),
                file: OutgoingFile { bytes: file.bytes, file_name: file.file_name, voice: false },
                caption,
                quote,
                mentions,
                styles,
            });
        }
        for command in commands {
            self.state.read(cx).send(command);
        }
        // Desktop's in-chat sound for a message we send (`sendMultiMediaMessage`).
        if self.state.read(cx).prefs.on(crate::prefs::Toggle::MessageSounds) {
            crate::sounds::play(crate::sounds::Sound::Whoosh);
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

    /// Whether what's staged can go view-once (Desktop's
    /// `isViewOnceEligible`): one photo or video, not a reply (and not
    /// while editing).
    fn view_once_eligible(&self) -> bool {
        let one_video = || {
            self.files.first().is_some_and(|f| {
                let content_type = signal_backend::media::content_type_for_file(&f.file_name);
                content_type.starts_with("video/") && signal_backend::mapping::can_view_once(content_type)
            })
        };
        self.reply_to.is_none()
            && self.editing.is_none()
            && self.images.len() + self.files.len() == 1
            && (self.images.len() == 1 || one_video())
    }

    /// Sends the one photo or video staged view-once; the text typed stays
    /// in the composer, since a view-once message carries none.
    fn send_view_once(&mut self, cx: &mut Context<Self>) {
        let media = if let Some(image) = self.images.pop() {
            image.release(cx);
            OutgoingViewOnce::Image(OutgoingImage { bytes: image.bytes, file_name: image.file_name })
        } else if let Some(file) = self.files.pop() {
            OutgoingViewOnce::Video(OutgoingFile { bytes: file.bytes, file_name: file.file_name, voice: false })
        } else {
            return;
        };
        self.view_once = false;
        self.state.read(cx).send(Command::SendViewOnce { conversation: self.conversation.clone(), media });
        cx.emit(ComposerEvent::Sent);
        cx.notify();
    }

    fn render_view_once_toggle(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.view_once;
        div()
            .id("view-once-toggle")
            .debug_selector(|| "view-once-toggle".into())
            .mx_3()
            .mt_2()
            .flex()
            .items_center()
            .gap_1p5()
            .w_auto()
            .self_start()
            .px_2()
            .py_1()
            .rounded_full()
            .cursor_pointer()
            .text_xs()
            .when(on, |d| d.bg(palette.accent).text_color(palette.on_accent))
            .when(!on, |d| d.border_1().border_color(palette.panel_border).text_color(palette.text_secondary))
            .child(Icon::new(IconName::CircleDashed).size_3p5())
            .child("View once")
            .on_click(cx.listener(|this, _, window, cx| {
                use gpui_kit::component::WindowExt as _;
                use gpui_kit::component::notification::Notification;
                this.view_once = !this.view_once;
                // Desktop's toast (`Toast--viewOnceEnabled` / `Disabled`).
                let note = if this.view_once { "View once enabled" } else { "View once disabled" };
                window.push_notification(Notification::new().message(note), cx);
                cx.notify();
            }))
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
        if signal_backend::too_big_to_send(bytes.len()) {
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
                    let limit = signal_backend::size_limit_label(signal_backend::remote_config().attachment_max_bytes());
                    problems.push(format!("{too_big} over Signal's {limit} limit"));
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

    /// The paperclip picks files. Where polls can be made it opens Desktop's
    /// attach menu instead (`CompositionArea`): File, and Poll.
    fn render_attach_button(&self, palette: Palette, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let polls = !self.conversation.is_signal()
            && self.state.read(cx).conversation(&self.conversation).is_some_and(|c| signal_backend::can_create_poll(c.kind));
        if !polls {
            return div()
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
                .child(Icon::new(IconName::Paperclip).size_5().text_color(palette.text_secondary))
                .into_any_element();
        }
        let this = cx.entity().downgrade();
        let (state, conversation) = (self.state.clone(), self.conversation.clone());
        Button::new("attach-button")
            .ghost()
            .rounded_full()
            .icon(Icon::new(IconName::Paperclip).size_5().text_color(palette.text_secondary))
            .tooltip("Add attachment or poll")
            .dropdown_menu(move |menu, _, _| {
                let (this, state, conversation) = (this.clone(), state.clone(), conversation.clone());
                menu.item(PopupMenuItem::new("File").icon(IconName::File).on_click(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.pick_files(window, cx));
                }))
                .item(PopupMenuItem::new("Poll").icon(IconName::ChartBar).on_click(move |_, window, cx| {
                    crate::views::poll::open_create(state.clone(), conversation.clone(), window, cx)
                }))
            })
            .into_any_element()
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
                    .child(div().text_sm().truncate().text_color(palette.text_secondary).child(signal_store::hide_spoilers(&message.body, &message.styles))),
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
        // As the quote will show: its text, else what it holds ("Voice
        // Message"), with a photo or Desktop's microphone beside it.
        let text = signal_store::hide_spoilers(&message.body, &message.styles);
        let attachment = message.attachments.first();
        let label = attachment.map(|a| signal_store::QuotedAttachment::of(a).label()).filter(|_| text.trim().is_empty());
        let thumbnail = message.thumbnail();
        let audio = thumbnail.is_none() && attachment.is_some_and(|a| a.content_type.starts_with("audio/"));
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
                    .map(|d| match label {
                        Some(label) => d.child(div().text_sm().italic().truncate().text_color(palette.text_secondary).child(label)),
                        None => d.child(div().text_sm().truncate().text_color(palette.text_secondary).child(text)),
                    }),
            )
            .when_some(thumbnail, |d, path| {
                d.child(img(PathBuf::from(path)).flex_none().size(px(40.)).rounded_md().object_fit(ObjectFit::Cover))
            })
            .when(audio, |d| {
                d.child(
                    div()
                        .flex_none()
                        .size(px(32.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(palette.background)
                        .child(Icon::new(IconName::Mic).size_4().text_color(palette.accent)),
                )
            })
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
/// with who each one mentions, in order, and moves `styles` (spans over
/// `text`) to match. A mention you've since edited stays as plain text.
fn encode_mentions(
    text: &str,
    picked: &[(String, String)],
    styles: &[signal_store::StyleSpan],
) -> (String, Vec<String>, Vec<signal_store::StyleSpan>) {
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
    // Where a position in `text` lands in `body`: each mention before it
    // shrinks to a placeholder; one inside a mention moves to its edge.
    let placeholder = signal_backend::mapping::MENTION_PLACEHOLDER.len_utf8();
    let moved = |at: usize, end: bool| {
        let mut shift = 0usize;
        let mut at = at;
        for (start, len, _) in &hits {
            if *start < at && at < start + len {
                at = if end { start + len } else { *start };
            }
            if start + len <= at {
                shift += len - placeholder;
            }
        }
        at - shift
    };
    let styles = styles
        .iter()
        .filter_map(|s| {
            let (start, end) = (moved(s.start, false), moved(s.start + s.len, true));
            (end > start).then_some(signal_store::StyleSpan { start, len: end - start, style: s.style })
        })
        .collect();
    (body, hits.into_iter().map(|(_, _, aci)| aci.to_owned()).collect(), styles)
}

impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.recording.is_some() {
            return self.recording_focus.clone();
        }
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let typeahead_open = self.typeahead.is_some();
        let can_send =
            !self.input.read(cx).value().trim().is_empty() || !self.images.is_empty() || !self.files.is_empty();

        use signal_store::TextStyle;
        div()
            .key_context("Composer")
            .on_action(cx.listener(|this, _: &FormatBold, window, cx| this.format(TextStyle::Bold, window, cx)))
            .on_action(cx.listener(|this, _: &FormatItalic, window, cx| this.format(TextStyle::Italic, window, cx)))
            .on_action(cx.listener(|this, _: &FormatStrikethrough, window, cx| this.format(TextStyle::Strikethrough, window, cx)))
            .on_action(cx.listener(|this, _: &FormatMonospace, window, cx| this.format(TextStyle::Monospace, window, cx)))
            .on_action(cx.listener(|this, _: &FormatSpoiler, window, cx| this.format(TextStyle::Spoiler, window, cx)))
            .on_action(cx.listener(|this, _: &RecordVoice, window, cx| this.start_recording(window, cx)))
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
            .capture_action(cx.listener(|this, enter: &Enter, window, cx| {
                // Only the message input's (the emoji picker's search, say,
                // keeps its own).
                if enter.shift || !this.input.read(cx).focus_handle(cx).is_focused(window) {
                    return;
                }
                // Handled before the input sees it: it would first replace a
                // selection (a word just made bold, say) with a line break.
                cx.stop_propagation();
                if this.typeahead.is_some() {
                    this.accept_typeahead(None, window, cx);
                } else {
                    this.submit(window, cx);
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
            .when(self.view_once_eligible(), |d| d.child(self.render_view_once_toggle(palette, cx)))
            .when_some(self.recording.as_ref().filter(|_| self.voice_draft.is_none()), |d, recording| {
                d.child(self.render_recording(recording, palette, cx))
            })
            .when(self.voice_draft.is_some(), |d| d.child(self.render_draft(palette, cx)))
            .when(self.recording.is_none(), |d| d.child(
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
                    .child(self.render_attach_button(palette, cx))
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
                    })
                    // Desktop's microphone, where Send goes once there's
                    // something to send.
                    .when(!can_send && self.editing.is_none(), |d| {
                        d.child(
                            div()
                                .id("mic-button")
                                .debug_selector(|| "mic-button".into())
                                .flex_none()
                                .size(px(32.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .cursor_pointer()
                                .hover(|d| d.bg(palette.row_hover))
                                .on_click(cx.listener(|this, _, window, cx| this.start_recording(window, cx)))
                                .child(Icon::new(IconName::Mic).size_5().text_color(palette.text_secondary)),
                        )
                    }),
            ))
    }
}

impl Composer {
    /// Desktop's `CompositionRecordingDraft`: play or pause, the waveform
    /// (click to play from there), the time, then Cancel and Send.
    fn render_draft(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        let draft = self.voice_draft.as_ref();
        let player = draft.and_then(|d| d.player.as_ref());
        let playing = player.is_some_and(|p| p.is_playing());
        let length = draft.and_then(|d| d.wave.as_ref()).map_or(0., |(_, seconds)| *seconds);
        let position = player.map_or(0., |p| p.position());
        let played = if length > 0. { (position / length).clamp(0., 1.) } else { 0. };
        let peaks = draft.and_then(|d| d.wave.as_ref()).map(|(peaks, _)| peaks.clone());
        let bars = (0..crate::audio::BARS).map(|bar| {
            let peak = peaks.as_ref().and_then(|p| p.get(bar).copied()).unwrap_or(0.);
            let done = (bar as f64 + 0.5) / crate::audio::BARS as f64 <= played;
            div()
                .id(("voice-draft-bar", bar))
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.seek_draft(bar as f64 / crate::audio::BARS as f64, window, cx);
                }))
                .child(
                    div()
                        .w(px(2.))
                        .h(px(2. + peak * 18.))
                        .rounded(px(1.))
                        .bg(if done { palette.text } else { palette.text_secondary }),
                )
        });
        div()
            .id("voice-draft")
            .debug_selector(|| "voice-draft".into())
            .track_focus(&self.recording_focus)
            .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.cancel_recording(window, cx);
                    cx.stop_propagation();
                }
            }))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .child(
                div()
                    .id("voice-draft-play")
                    .flex_none()
                    .size(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .bg(palette.input_bg)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_draft(window, cx)))
                    .child(Icon::new(if playing { IconName::Pause } else { IconName::Play }).size_4().text_color(palette.text)),
            )
            .child(div().flex_1().min_w_0().h(px(32.)).flex().items_center().children(bars))
            .child(
                div()
                    .flex_none()
                    .min_w(px(36.))
                    .text_sm()
                    .text_color(palette.text_secondary)
                    .child(crate::audio::clock(if playing { position } else { length })),
            )
            .child(
                Button::new("voice-draft-cancel")
                    .label("Cancel")
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_recording(window, cx))),
            )
            .child(
                Button::new("voice-draft-send")
                    .label("Send")
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.send_recording(window, cx))),
            )
    }

    /// Desktop's `CompositionRecording`: a red dot, the time, the level as
    /// bars scrolling in from the right, then Cancel and Send.
    fn render_recording(&self, recording: &Recording, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        let red = gpui_kit::Hsla { h: 0., s: 0.75, l: 0.5, a: 1. };
        // Newest first: the row runs right to left.
        let bars = recording.levels.iter().rev().enumerate().map(|(ix, level)| {
            div()
                .id(("voice-level", ix))
                .flex_none()
                .w(px(2.))
                .min_h(px(2.))
                .h(gpui_kit::relative(level.clamp(0., 1.)))
                .rounded(px(1.))
                .bg(palette.text_secondary)
        });
        div()
            .id("voice-recording")
            .track_focus(&self.recording_focus)
            .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                // Desktop's: Esc cancels (`useEscapeHandling`).
                if event.keystroke.key == "escape" {
                    this.cancel_recording(window, cx);
                    cx.stop_propagation();
                }
            }))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .child(div().flex_none().size(px(10.)).rounded_full().bg(red))
            .child(
                div()
                    .flex_none()
                    .min_w(px(36.))
                    .text_sm()
                    .text_color(palette.text)
                    .child(crate::audio::clock(recording.elapsed().floor())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(32.))
                    .px_2()
                    .py_1()
                    .overflow_hidden()
                    .rounded(px(6.))
                    .bg(palette.input_bg)
                    .flex()
                    .flex_row_reverse()
                    .items_center()
                    .gap(px(2.))
                    .children(bars),
            )
            .child(
                Button::new("voice-cancel")
                    .label("Cancel")
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_recording(window, cx))),
            )
            .child(
                Button::new("voice-send")
                    .label("Send")
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.send_recording(window, cx))),
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
        let (body, acis, _) = encode_mentions("@Bob and @Alice Liddell, @Bob", &[alice.clone(), bob.clone(), bob.clone()], &[]);
        assert_eq!(body, "\u{FFFC} and \u{FFFC}, \u{FFFC}");
        assert_eq!(acis, ["b", "a", "b"]);
        // Edited away: plain text.
        let (body, acis, _) = encode_mentions("@Ali", &[alice], &[]);
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
    fn typed_emoticons_become_emoji_unless_turned_off(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        let type_in = |text: &'static str, cx: &mut TestAppContext| {
            cx.update_window(f.window, |_, window, cx| {
                window.click(target.clone(), cx);
                window.input(text, cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        type_in("see you <3", cx);
        assert_eq!(text(&f, cx), "see you ❤️");
        // Turned off (Settings, Chats), they stay as typed.
        let state = cx.update(|cx| f.composer.read(cx).state.clone());
        state.update(cx, |s, cx| s.set_auto_convert_emoji(false, cx));
        type_in(" :-)", cx);
        assert_eq!(text(&f, cx), "see you ❤️ :-)");
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
            assert!(!c.add_file(vec![0; signal_backend::remote_config().attachment_max_bytes() as usize + 1], Some("big.bin".into()), cx));
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
    fn text_over_64_kib_isnt_sent(cx: &mut TestAppContext) {
        let f = mount(cx);
        let long = "a".repeat(signal_backend::long_text::MAX_TEXT_BYTES + 1);
        cx.update_window(f.window, |_, window, cx| {
            f.composer.update(cx, |c, cx| {
                c.input.update(cx, |input, cx| input.set_value(long.clone(), window, cx));
                c.submit(window, cx);
            });
        })
        .unwrap();
        // Kept, to be shortened, as Desktop's composer refuses it.
        assert_eq!(text(&f, cx).len(), long.len());
    }

    #[gpui_kit::test]
    fn a_sticker_sent_while_replying_carries_the_reply(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("the original", cx);
            f.composer.update(cx, |c, cx| c.submit(window, cx));
        })
        .unwrap();
        let original =
            sent(&f, |m| m.body == "the original").into_iter().find(|m| m.body == "the original").unwrap();
        let path = f._dir.path().join("sticker.png");
        std::fs::write(&path, b"made up").unwrap();
        let pack = std::sync::Arc::new(signal_backend::StickerPackView {
            id: vec![1],
            key: vec![0; 32],
            title: "Pack".into(),
            author: "Someone".into(),
            stickers: vec![signal_backend::StickerView { id: 0, emoji: Some("🔥".into()), path: path.clone() }],
        });
        let send_sticker = |cx: &mut TestAppContext| {
            cx.update_window(f.window, |_, _, cx| {
                f.composer.update(cx, |c, _| c.picker_open = true);
                let picker = f.composer.read(cx).picker.clone();
                picker.update(cx, |_, cx| cx.emit(PickerEvent::Sticker(pack.clone(), pack.stickers[0].clone())));
            })
            .unwrap();
            cx.run_until_parked();
        };
        let replying = |cx: &mut TestAppContext| cx.update(|cx| f.composer.read(cx).reply_to.as_ref().map(|m| m.id));

        // With a draft to send, the reply stays for it, as in Desktop.
        cx.update_window(f.window, |_, window, cx| {
            f.composer.update(cx, |c, cx| c.reply_to(original.clone(), window, cx));
            window.input("still typing", cx);
        })
        .unwrap();
        send_sticker(cx);
        assert_eq!(replying(cx), Some(original.id));
        let stickers = |f: &Fixture| {
            sent(f, |m| m.attachments.iter().any(|a| a.sticker))
                .into_iter()
                .filter(|m| m.attachments.iter().any(|a| a.sticker))
                .collect::<Vec<_>>()
        };
        let first = stickers(&f);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].quote.as_ref().map(|q| (q.sent_at, q.text.as_str())), Some((original.sent_at, "the original")));

        // Without one, the sticker takes the reply with it.
        cx.update_window(f.window, |_, window, cx| {
            f.composer.update(cx, |c, cx| c.input.update(cx, |input, cx| input.set_value("", window, cx)));
        })
        .unwrap();
        send_sticker(cx);
        assert_eq!(replying(cx), None);
        for _ in 0..300 {
            if stickers(&f).len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(stickers(&f).iter().all(|m| m.quote.as_ref().is_some_and(|q| q.sent_at == original.sent_at)));
    }

    /// The GIF itself would come from Giphy, which tests don't reach (the
    /// backend refuses this made-up URL), so this checks the composer's
    /// side: the reply goes with the GIF as with a sticker.
    #[gpui_kit::test]
    fn a_gif_sent_while_replying_takes_the_reply(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        cx.update_window(f.window, |_, window, cx| {
            window.click(target, cx);
            window.input("the original", cx);
            f.composer.update(cx, |c, cx| c.submit(window, cx));
        })
        .unwrap();
        let original =
            sent(&f, |m| m.body == "the original").into_iter().find(|m| m.body == "the original").unwrap();
        let media = signal_backend::giphy::GifMedia { url: "https://example.invalid/gif.mp4".into(), width: 200, height: 150 };
        let gif = signal_backend::giphy::Gif { id: "g".into(), title: "A GIF".into(), preview: media.clone(), original: media };
        let send_gif = |cx: &mut TestAppContext| {
            cx.update_window(f.window, |_, _, cx| {
                f.composer.update(cx, |c, _| c.picker_open = true);
                let picker = f.composer.read(cx).picker.clone();
                picker.update(cx, |_, cx| cx.emit(PickerEvent::Gif(gif.clone())));
            })
            .unwrap();
            cx.run_until_parked();
        };
        let replying = |cx: &mut TestAppContext| cx.update(|cx| f.composer.read(cx).reply_to.as_ref().map(|m| m.id));
        cx.update_window(f.window, |_, window, cx| {
            f.composer.update(cx, |c, cx| c.reply_to(original.clone(), window, cx));
            window.input("still typing", cx);
        })
        .unwrap();
        send_gif(cx);
        assert_eq!(replying(cx), Some(original.id), "kept for the draft");
        cx.update_window(f.window, |_, window, cx| {
            f.composer.update(cx, |c, cx| c.input.update(cx, |input, cx| input.set_value("", window, cx)));
        })
        .unwrap();
        send_gif(cx);
        assert_eq!(replying(cx), None, "went with the GIF");
    }

    thread_local! {
        /// The stand-in microphone the next recording gets.
        static MIC: std::cell::RefCell<crate::recorder::fake::FakeMic> = Default::default();
    }

    fn fake_microphone(path: &std::path::Path) -> anyhow::Result<Box<dyn crate::recorder::Recorder>> {
        std::fs::write(path, b"not really AAC")?;
        Ok(Box::new(MIC.with(|m| m.borrow().clone())))
    }

    fn start_fake_recording(f: &Fixture, cx: &mut gpui_kit::VisualTestContext) -> crate::recorder::fake::FakeMic {
        let mic = crate::recorder::fake::FakeMic::default();
        MIC.with(|m| *m.borrow_mut() = mic.clone());
        cx.update(|_, cx| {
            let state = f.composer.read(cx).state.clone();
            state.update(cx, |s, _| s.start_recorder = fake_microphone);
        });
        cx.run_until_parked();
        let button = cx.debug_bounds("mic-button").expect("the microphone shows in an empty composer").center();
        cx.simulate_click(button, gpui_kit::Modifiers::none());
        cx.run_until_parked();
        mic
    }

    #[gpui_kit::test]
    fn a_recording_sends_as_a_voice_message(cx: &mut TestAppContext) {
        let f = mount(cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        let mic = start_fake_recording(&f, &mut cx);
        let path = cx.update(|_, cx| f.composer.read(cx).recording.as_ref().map(|r| r.path.clone())).expect("recording");
        mic.level.set(0.5);
        mic.seconds.set(3.);
        cx.executor().advance_clock(RECORDING_POLL * 3);
        cx.run_until_parked();
        let levels = cx.update(|_, cx| f.composer.read(cx).recording.as_ref().unwrap().levels.len());
        assert!(levels >= 2, "levels are read as it records ({levels})");

        cx.update(|window, cx| f.composer.update(cx, |c, cx| c.send_recording(window, cx)));
        assert!(mic.stopped.get() && !path.exists(), "stopped, and the recording handed over");
        assert!(cx.update(|_, cx| f.composer.read(cx).recording.is_none()));
        let messages = sent(&f, |m| m.attachments.iter().any(|a| a.voice));
        let voice = messages.iter().find(|m| m.attachments.iter().any(|a| a.voice)).unwrap();
        let attachment = &voice.attachments[0];
        // As phones send them: AAC, without a name.
        assert_eq!((attachment.content_type.as_str(), attachment.file_name.as_deref()), ("audio/aac", None));
        assert_eq!(std::fs::read(attachment.local_path.as_ref().unwrap()).unwrap(), b"not really AAC");
    }

    #[gpui_kit::test]
    fn leaving_the_app_keeps_the_recording_as_a_draft(cx: &mut TestAppContext) {
        let f = mount(cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        let mic = start_fake_recording(&f, &mut cx);
        mic.seconds.set(2.);
        // What the window losing focus does (no audio is played here).
        f.composer.update(&mut cx, |c, cx| c.save_draft_recording(cx));
        cx.run_until_parked();
        assert!(mic.stopped.get(), "recording stops");
        assert!(cx.update(|_, cx| f.composer.read(cx).voice_draft.is_some()));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("voice-draft").is_some(), "the draft, to play back");
        // Sent from the draft, as a voice message.
        cx.update(|window, cx| f.composer.update(cx, |c, cx| c.send_recording(window, cx)));
        assert!(cx.update(|_, cx| f.composer.read(cx).voice_draft.is_none()));
        let messages = sent(&f, |m| m.attachments.iter().any(|a| a.voice));
        assert!(messages.iter().any(|m| m.attachments.iter().any(|a| a.voice)));
    }

    #[gpui_kit::test]
    fn escape_cancels_a_recording(cx: &mut TestAppContext) {
        let f = mount(cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        let mic = start_fake_recording(&f, &mut cx);
        let path = cx.update(|_, cx| f.composer.read(cx).recording.as_ref().map(|r| r.path.clone())).expect("recording");
        cx.update(|window, cx| window.press("escape", cx));
        cx.run_until_parked();
        assert!(cx.update(|_, cx| f.composer.read(cx).recording.is_none()));
        assert!(mic.stopped.get() && !path.exists(), "stopped and thrown away");
    }

    #[gpui_kit::test]
    fn an_hour_stops_the_recording_and_asks(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        let f = mount(cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        let mic = start_fake_recording(&f, &mut cx);
        mic.seconds.set(crate::recorder::MAX_SECONDS);
        cx.executor().advance_clock(RECORDING_POLL * 2);
        cx.run_until_parked();
        assert!(mic.stopped.get(), "the microphone stops at the limit");
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)), "Desktop's Discard / Send anyway");
        // Still there to send until one is picked.
        assert!(cx.update(|_, cx| f.composer.read(cx).recording.as_ref().is_some_and(|r| r.stopped)));
        // Cancelled behind the dialog: the dialog goes with it.
        cx.update(|window, cx| f.composer.update(cx, |c, cx| c.cancel_recording(window, cx)));
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    }

    #[gpui_kit::test]
    fn the_microphone_gives_way_to_send(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        cx.run_until_parked();
        assert!(cx.debug_bounds("mic-button").is_some());
        cx.update(|window, cx| {
            window.click(target, cx);
            window.input("hello", cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("mic-button").is_none(), "Send takes its place");
        // A voice message goes alone: not with a file staged.
        f.composer.update(&mut cx, |c, cx| assert!(c.add_file(b"x".to_vec(), Some("a.txt".into()), cx)));
        cx.update(|window, cx| f.composer.update(cx, |c, cx| c.start_recording(window, cx)));
        assert!(cx.update(|_, cx| f.composer.read(cx).recording.is_none()));
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

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(8, 8).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        bytes
    }

    #[gpui_kit::test]
    fn view_once_is_offered_for_one_photo_or_video_only(cx: &mut TestAppContext) {
        let f = mount(cx);
        let eligible = |cx: &mut TestAppContext| cx.update(|cx| f.composer.read(cx).view_once_eligible());
        assert!(!eligible(cx));
        f.composer.update(cx, |c, cx| assert!(c.add_image(png(), None, cx)));
        assert!(eligible(cx));
        // Two photos: no (Desktop's `isViewOnceEligible`).
        f.composer.update(cx, |c, cx| assert!(c.add_image(png(), None, cx)));
        assert!(!eligible(cx));
        f.composer.update(cx, |c, cx| {
            c.images.drain(..).for_each(|i| i.release(cx));
            assert!(c.add_file(b"not really".to_vec(), Some("clip.mp4".into()), cx));
        });
        assert!(eligible(cx), "a video");
        f.composer.update(cx, |c, cx| {
            c.files.clear();
            assert!(c.add_file(b"%PDF-1.7".to_vec(), Some("notes.pdf".into()), cx));
        });
        assert!(!eligible(cx), "not a photo or video");
    }

    #[gpui_kit::test]
    fn a_view_once_photo_goes_without_the_text(cx: &mut TestAppContext) {
        let f = mount(cx);
        let target = input_id(&f, cx);
        let mut cx = gpui_kit::VisualTestContext::from_window(f.window, cx);
        cx.update(|window, cx| {
            window.click(target, cx);
            window.input("keep this", cx);
        });
        f.composer.update(&mut cx, |c, cx| assert!(c.add_image(png(), Some("dot.png".into()), cx)));
        cx.run_until_parked();
        let toggle = cx.debug_bounds("view-once-toggle").expect("offered for one photo");
        cx.simulate_click(toggle.center(), gpui_kit::Modifiers::none());
        assert!(cx.update(|_, cx| f.composer.read(cx).view_once));
        cx.update(|window, cx| f.composer.update(cx, |c, cx| c.submit(window, cx)));

        let messages = sent(&f, |m| m.view_once.is_some());
        let once = messages.iter().find(|m| m.view_once.is_some()).unwrap();
        // The demo sends at once, and ours is erased once sent.
        assert_eq!(once.view_once, Some(signal_store::ViewOnce::Gone));
        assert!(once.body.is_empty() && once.attachments.is_empty());
        // The text stays for the next message; the toggle is off again.
        let (text, view_once, images) = cx.update(|_, cx| {
            let c = f.composer.read(cx);
            (c.input.read(cx).value().to_string(), c.view_once, c.images.len())
        });
        assert_eq!((text.as_str(), view_once, images), ("keep this", false, 0));
    }
}
