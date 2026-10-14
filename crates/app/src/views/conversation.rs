//! Conversation pane: header, message timeline and composer.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::*;
use gpui_kit::{
    Anchor, AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, FollowMode, FontWeight, Hsla,
    ListAlignment, ListOffset, ListScrollEvent, ListState, SharedString, Subscription, Window, deferred, div, img,
    anchored, list, px,
};
use signal_backend::Command;
use signal_store::{
    AttachmentMeta, ConversationId, ConversationKind, DeliveryStatus, Direction, Message, Reaction,
};

use crate::state::{AppEvent, AppState};
use crate::image_cache::LruImageCache;
use crate::views::image_viewer::ViewedImage;
use crate::theme::{Palette, color_for};
use crate::views::common::{avatar, person_avatar, collapse_whitespace, find_links, conversation_avatar, day_label, message_time, same_day, status_icon, StatusPlace};
use crate::views::composer::{Composer, ComposerEvent};
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};

const PAGE: u32 = 60;
/// Consecutive messages from one author within this window are grouped.
const GROUP_WINDOW_MS: i64 = 3 * 60 * 1000;
/// Signal's default reaction set.
const QUICK_REACTIONS: [&str; 6] = ["❤️", "👍", "👎", "😂", "😮", "😢"];

pub struct ConversationView {
    state: Entity<AppState>,
    pub id: ConversationId,
    messages: Vec<Message>,
    list: ListState,
    composer: Entity<Composer>,
    picker: Entity<EmojiPicker>,
    loading_older: bool,
    has_more: bool,
    hovered: Option<i64>,
    reaction_bar_for: Option<i64>,
    reaction_picker_for: Option<i64>,
    /// Incoming messages that arrived while scrolled up (shown on the
    /// jump-to-latest button); they're read once scrolled into view.
    unseen: usize,
    /// Messages are only marked read while you can see them.
    window_active: bool,
    /// Decoded photos for this conversation, bounded; freed with the view.
    image_cache: Entity<LruImageCache>,
    /// The reaction viewer: which message, and which emoji tab (`None` = All).
    reaction_viewer: Option<(i64, Option<String>)>,
    /// Newer messages exist past the last loaded one (after jumping back).
    has_newer: bool,
    /// Bumped by each full reload; page results from an older one are stale.
    generation: u64,
    loading_newer: bool,
    /// A message just jumped to, highlighted briefly.
    highlight: Option<i64>,
    /// The message whose "⋯" menu is open (its hover buttons stay up).
    menu_open_for: Option<i64>,
    /// Select mode: the chosen messages.
    selecting: Option<std::collections::BTreeSet<i64>>,
    /// Pinned messages, newest pin first, and which one the bar shows.
    pins: Vec<Message>,
    pin_index: usize,
    _subscriptions: Vec<Subscription>,
}

/// How long a jumped-to message stays highlighted.
const HIGHLIGHT: std::time::Duration = std::time::Duration::from_secs(2);

impl ConversationView {
    pub fn new(
        state: Entity<AppState>,
        picker: Entity<EmojiPicker>,
        id: ConversationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = state.read(cx).conversation(&id).and_then(|c| c.draft.clone());
        let composer = cx.new(|cx| Composer::new(state.clone(), picker.clone(), id.clone(), draft, window, cx));

        let list = ListState::new(0, ListAlignment::Top, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |event: &ListScrollEvent, _, cx| {
            let weak = weak.clone();
            let near_top = event.visible_range.start < 8;
            let near_end = event.visible_range.end + 8 >= event.count;
            let at_end = event.visible_range.end >= event.count;
            cx.defer(move |cx| {
                let _ = weak.update(cx, |this, cx| {
                    if near_top {
                        this.load_older(cx);
                    }
                    if near_end {
                        this.load_newer(cx);
                    }
                    // Scrolled down to the newest messages: they've been seen.
                    if at_end && !this.has_newer {
                        this.caught_up(cx);
                    }
                    cx.notify();
                });
            });
        });

        let subscriptions = vec![
            cx.subscribe(&state, |this, _, event: &AppEvent, cx| match event {
                AppEvent::MessageChanged { conversation, message_id } if *conversation == this.id => {
                    this.on_message_changed(*message_id, cx);
                }
                AppEvent::MessageRemoved { conversation, message_id } if *conversation == this.id => {
                    this.on_message_removed(*message_id, cx);
                    if this.pins.iter().any(|p| p.id == *message_id) {
                        this.load_pins(cx);
                    }
                }
                AppEvent::ConversationCleared(conversation) if *conversation == this.id => this.load_initial(cx),
                AppEvent::PinsChanged(conversation) if *conversation == this.id => this.load_pins(cx),
                AppEvent::JumpTo { conversation } if *conversation == this.id => {
                    if let Some(id) = this.state.update(cx, |state, _| state.take_jump(conversation)) {
                        this.load_around(id, cx);
                    }
                }
                AppEvent::TypingChanged { conversation } if *conversation == this.id => cx.notify(),
                AppEvent::ConversationsChanged => cx.notify(),
                AppEvent::AuthorNamesChanged(names) => this.rename_authors(names, cx),
                _ => {}
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                this.window_active = window.is_window_active();
                // Coming back to the window at the bottom of a chat reads it.
                if this.window_active && this.at_end() {
                    this.caught_up(cx);
                }
            }),
            cx.subscribe_in(&composer, window, |this, _, event: &ComposerEvent, window, cx| match event {
                ComposerEvent::Sent => {
                    this.list.set_follow_mode(FollowMode::Tail);
                    this.list.scroll_to_end();
                    cx.notify();
                }
                ComposerEvent::EditLast => {
                    let note_to_self = this.note_to_self(cx);
                    let now = now_millis();
                    let last = this
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.direction == Direction::Outgoing)
                        .filter(|m| signal_backend::edits::can_edit(m, note_to_self, now))
                        .cloned();
                    if let Some(message) = last {
                        this.composer.update(cx, |composer, cx| composer.start_edit(message, window, cx));
                    }
                }
            }),
            cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, window, cx| {
                let Some(message_id) = this.reaction_picker_for else { return };
                match event {
                    PickerEvent::Picked(glyph) => {
                        this.react(message_id, glyph.clone(), cx);
                        this.reaction_picker_for = None;
                    }
                    PickerEvent::Dismissed | PickerEvent::Sticker(..) | PickerEvent::Gif(_) => {
                        this.reaction_picker_for = None
                    }
                }
                let handle = this.composer.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                cx.notify();
            }),
        ];

        let mut this = Self {
            state,
            id,
            messages: Vec::new(),
            list,
            composer,
            picker,
            loading_older: false,
            has_more: true,
            hovered: None,
            reaction_bar_for: None,
            reaction_picker_for: None,
            reaction_viewer: None,
            unseen: 0,
            window_active: window.is_window_active(),
            image_cache: LruImageCache::new(cx),
            has_newer: false,
            generation: 0,
            loading_newer: false,
            highlight: None,
            menu_open_for: None,
            selecting: None,
            pins: Vec::new(),
            pin_index: 0,
            _subscriptions: subscriptions,
        };
        let jump = this.state.update(cx, |state, _| state.take_jump(&this.id));
        match jump {
            Some(message_id) => this.load_around(message_id, cx),
            None => this.load_initial(cx),
        }
        this.mark_read(cx);
        this.load_pins(cx);
        this
    }

    fn load_pins(&mut self, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move { db.pinned_messages(&id).await });
        cx.spawn(async move |this, cx| {
            let pins = query.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.pins = pins;
                this.pin_index = 0;
                cx.notify();
            });
        })
        .detach();
    }

    /// Shows the message `author` sent at `sent_at` (from a notice).
    fn go_to(&mut self, author: Option<String>, sent_at: i64, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move { db.find_message(&id, author.as_deref(), sent_at).await });
        cx.spawn(async move |this, cx| {
            let found = query.await.ok().flatten();
            let _ = this.update(cx, |this, cx| match found {
                Some(message) => this.load_around(message.id, cx),
                None => this.state.update(cx, |state, cx| state.report_error("Pinned message not found".to_owned(), cx)),
            });
        })
        .detach();
    }

    /// "You pinned a message", "Alice set the disappearing message time to
    /// 1 day.": centered, between messages.
    fn render_notice(&self, notice: &signal_store::Notice, show_day: bool, sent_at: i64, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use signal_store::Notice;
        let text = crate::views::notices::notice_text(notice, |sid| self.reactor_name(Some(sid), cx));
        let go_to = match notice {
            Notice::Pinned { author, sent_at, .. } => Some((author.clone(), *sent_at)),
            _ => None,
        };
        // Their safety number (they may have no 1:1 chat, only a group).
        let safety_with = match notice {
            Notice::SafetyNumberChanged { who } => Some((ConversationId::direct(who), self.reactor_name(Some(who), cx))),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .w_full()
            .when(show_day, |d| {
                d.child(
                    div()
                        .pt_4()
                        .pb_2()
                        .flex()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.text_secondary)
                        .child(day_label(sent_at)),
                )
            })
            .child(
                div()
                    .py_2()
                    .px_6()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .text_color(palette.text_secondary)
                    .child(div().text_center().child(text))
                    .when_some(go_to, |d, (author, at)| {
                        d.child(
                            Button::new(SharedString::from(format!("notice-go-{at}")))
                                .label("Go to message")
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| this.go_to(author.clone(), at, cx))),
                        )
                    })
                    .when_some(safety_with, |d, (chat, name)| {
                        let state = self.state.clone();
                        d.child(
                            Button::new(SharedString::from(format!("notice-safety-{sent_at}")))
                                .label("View safety number")
                                .ghost()
                                .on_click(move |_, window, cx| {
                                    crate::views::safety_number::open(state.clone(), chat.clone(), name.clone(), window, cx);
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Desktop's pinned messages bar: the current pin's preview; a click
    /// shows it and moves to the next.
    fn render_pins(&self, palette: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        if self.pins.is_empty() {
            return None;
        }
        let count = self.pins.len();
        let ix = self.pin_index % count;
        let pin = &self.pins[ix];
        let id = pin.id;
        let author = self.reactor_name(pin.author.as_deref(), cx);
        let preview = if !pin.body.is_empty() {
            collapse_whitespace(&pin.body)
        } else {
            match pin.attachments.first() {
                Some(a) if a.sticker => "Sticker".into(),
                Some(a) if a.gif => "GIF".into(),
                Some(a) if a.content_type.starts_with("image/") => "Photo".into(),
                Some(a) if a.content_type.starts_with("video/") => "Video".into(),
                Some(_) => "File".into(),
                None => String::new(),
            }
        };
        let (state, view) = (self.state.clone(), cx.entity().downgrade());
        Some(
            div()
                .id("pinned-bar")
                .debug_selector(|| "pinned-bar".into())
                .flex_none()
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(palette.panel_border)
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.load_around(id, cx);
                    this.pin_index = (ix + 1) % count;
                }))
                // One tab per pin, the shown one highlighted.
                .child(div().flex().flex_col().gap(px(2.)).h(px(32.)).children((0..count).map(|n| {
                    div().w(px(3.)).flex_1().rounded_full().bg(if n == ix { palette.accent } else { palette.panel_border })
                })))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(author))
                        .child(div().text_sm().truncate().text_color(palette.text_secondary).child(preview)),
                )
                .child(
                    Button::new("pinned-menu")
                        .ghost()
                        .icon(IconName::Ellipsis)
                        .tooltip("More actions")
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                            let (state, view) = (state.clone(), view.clone());
                            menu.item(
                                PopupMenuItem::new("Unpin")
                                    .icon(IconName::PinOff)
                                    .on_click(move |_, _, cx| state.read(cx).send(Command::UnpinMessage { message_id: id })),
                            )
                            .item(PopupMenuItem::new("Go to message").on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.load_around(id, cx));
                            }))
                        }),
                )
                .into_any_element(),
        )
    }

    fn mark_read(&self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        if state.conversation(&self.id).is_some_and(|c| c.unread_count > 0) {
            state.send(Command::MarkRead {
                conversation: self.id.clone(),
            });
        }
    }

    /// Showing the newest messages.
    /// Showing the newest messages (not just the end of what's loaded).
    fn at_end(&self) -> bool {
        !self.has_newer && (self.list.is_following_tail() || self.list.is_scrolled_to_end().unwrap_or(true))
    }

    /// The newest messages are on screen (and the window is in front): read
    /// them all.
    fn caught_up(&mut self, cx: &mut Context<Self>) {
        if !self.window_active {
            return;
        }
        let unread = self.state.read(cx).conversation(&self.id).is_some_and(|c| c.unread_count > 0);
        if unread || self.unseen > 0 {
            // Not gated on the cached count: a message that just arrived may
            // not be counted in the list we hold yet.
            self.state.read(cx).send(Command::MarkRead { conversation: self.id.clone() });
        }
        self.unseen = 0;
    }

    fn load_initial(&mut self, cx: &mut Context<Self>) {
        self.loading_older = true;
        self.generation += 1;
        let generation = self.generation;
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move { db.messages(&id, None, PAGE).await });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading_older = false;
                match result {
                    Ok(messages) => {
                        this.has_more = messages.len() as u32 == PAGE;
                        this.has_newer = false;
                        this.messages = messages;
                        this.list.reset(this.messages.len());
                        this.list.set_follow_mode(FollowMode::Tail);
                        this.list.scroll_to_end();
                    }
                    Err(e) => tracing::error!("loading messages: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Loads the messages around `message_id`, scrolls to it and
    /// highlights it (a search result).
    fn load_around(&mut self, message_id: i64, cx: &mut Context<Self>) {
        self.loading_older = true;
        self.generation += 1;
        let generation = self.generation;
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let half = PAGE / 2;
        let query = backend.spawn(async move { db.messages_around(&id, message_id, half, half).await });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading_older = false;
                let Ok(messages) = result else { return };
                let Some(ix) = messages.iter().position(|m| m.id == message_id) else { return };
                this.has_more = ix as u32 == half;
                this.has_newer = (messages.len() - ix - 1) as u32 == half;
                this.messages = messages;
                this.list.reset(this.messages.len());
                this.list.set_follow_mode(FollowMode::Normal);
                // A little context above it.
                this.list.scroll_to(ListOffset { item_ix: ix.saturating_sub(2), offset_in_item: px(0.) });
                this.highlight = Some(message_id);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(HIGHLIGHT).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.highlight == Some(message_id) {
                            this.highlight = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
                cx.notify();
            });
        })
        .detach();
    }

    /// The next page of newer messages, after jumping back.
    fn load_newer(&mut self, cx: &mut Context<Self>) {
        if self.loading_newer || !self.has_newer {
            return;
        }
        let Some(last) = self.messages.last().map(|m| m.id) else { return };
        self.loading_newer = true;
        let generation = self.generation;
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move { db.messages_after(&id, last, PAGE).await });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                this.loading_newer = false;
                if this.generation != generation {
                    return;
                }
                let Ok(newer) = result else { return };
                this.has_newer = newer.len() as u32 == PAGE;
                if newer.is_empty() {
                    return;
                }
                let start = this.messages.len();
                let count = newer.len();
                this.messages.extend(newer);
                this.list.splice(start..start, count);
                // The old last row may lose its time / corner.
                this.list.remeasure_items(start.saturating_sub(1)..start);
                cx.notify();
            });
        })
        .detach();
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        if self.loading_older || !self.has_more || self.messages.is_empty() {
            return;
        }
        self.loading_older = true;
        let generation = self.generation;
        let before = self.messages[0].id;
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move { db.messages(&id, Some(before), PAGE).await });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading_older = false;
                let Ok(older) = result else { return };
                this.has_more = older.len() as u32 == PAGE;
                if older.is_empty() {
                    return;
                }
                // Keep the viewport anchored on the same message.
                let anchor = this.list.logical_scroll_top();
                let count = older.len();
                this.messages.splice(0..0, older);
                this.list.splice(0..0, count);
                this.list.scroll_to(ListOffset {
                    item_ix: anchor.item_ix + count,
                    offset_in_item: anchor.offset_in_item,
                });
                // The old first row may lose its day separator / group header.
                this.list.remeasure_items(count..count + 1);
                cx.notify();
            });
        })
        .detach();
    }

    fn on_message_changed(&mut self, message_id: i64, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let query = backend.spawn(async move { db.message(message_id).await });
        cx.spawn(async move |this, cx| {
            let Ok(Some(message)) = query.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(ix) = this.messages.iter().position(|m| m.id == message.id) {
                    this.messages[ix] = message;
                    this.list.remeasure_items(ix..ix + 1);
                } else if this.has_newer
                    && this.messages.last().is_some_and(|last| (message.sent_at, message.id) > (last.sent_at, last.id))
                {
                    // Past what's loaded (we jumped back): it's waiting below.
                    if message.direction == Direction::Incoming {
                        this.unseen += 1;
                    }
                } else {
                    // New messages are almost always last.
                    // In the store's order (by time, then row).
                    let ix = this.messages.partition_point(|m| (m.sent_at, m.id) < (message.sent_at, message.id));
                    let incoming = message.direction == Direction::Incoming;
                    // Before the splice, which forgets the scroll position.
                    let was_at_end = this.at_end();
                    this.messages.insert(ix, message);
                    this.list.splice(ix..ix, 1);
                    if ix > 0 {
                        // Grouping of the previous row may have changed.
                        this.list.remeasure_items(ix - 1..ix);
                    }
                    if incoming {
                        // Read now if you're looking at the bottom of the chat;
                        // otherwise it waits for you to scroll down.
                        if was_at_end && this.window_active {
                            this.state.read(cx).send(Command::MarkRead { conversation: this.id.clone() });
                        } else {
                            this.unseen += 1;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_message_removed(&mut self, message_id: i64, cx: &mut Context<Self>) {
        let Some(ix) = self.messages.iter().position(|m| m.id == message_id) else { return };
        self.messages.remove(ix);
        self.list.splice(ix..ix + 1, 0);
        if ix > 0 {
            // Grouping of the neighbours may have changed.
            self.list.remeasure_items(ix - 1..(ix + 1).min(self.messages.len()));
        }
        if self.hovered == Some(message_id) {
            self.hovered = None;
        }
        cx.notify();
    }

    /// The store has already been updated; patch the loaded copies to match.
    fn rename_authors(&mut self, names: &[(String, String)], cx: &mut Context<Self>) {
        let name_of = |author: &str| names.iter().find(|(id, _)| id == author).map(|(_, name)| name.clone());
        for message in &mut self.messages {
            if let Some(name) = message.author.as_deref().and_then(name_of) {
                message.author_name = Some(name);
            }
            if let Some(quote) = &mut message.quote
                && let Some(name) = quote.author.as_deref().and_then(name_of)
            {
                quote.author_name = name;
            }
        }
        self.list.remeasure();
        cx.notify();
    }

    fn react(&mut self, message_id: i64, emoji: String, cx: &mut Context<Self>) {
        // Tapping your existing reaction removes it.
        let mine = self
            .messages
            .iter()
            .find(|m| m.id == message_id)
            .and_then(|m| m.reactions.iter().find(|r| r.author.is_none()))
            .map(|r| r.emoji.clone());
        let remove = mine.as_deref() == Some(emoji.as_str());
        self.state.update(cx, |state, _| {
            if !remove {
                state.record_emoji(&emoji);
            }
            state.send(Command::React {
                message_id,
                emoji,
                remove,
            });
        });
        self.reaction_bar_for = None;
        cx.notify();
    }

    fn render_header(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let conversation = self.state.read(cx).conversation(&self.id).cloned();
        let conversation = conversation.as_ref();
        let title = match conversation {
            Some(c) if c.kind == ConversationKind::NoteToSelf => "Note to Self".to_owned(),
            Some(c) => c.title.clone(),
            None => String::new(),
        };
        let subtitle = conversation.and_then(|c| match c.kind {
            ConversationKind::Direct => c.phone.clone(),
            ConversationKind::Group => Some("Group".into()),
            ConversationKind::NoteToSelf => None,
        });
        let timer = conversation.map(|c| c.expire_timer).unwrap_or(0);
        let color = conversation.map(|c| c.color).unwrap_or(0);

        div()
            .h(px(52.))
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .border_b_1()
            .border_color(palette.panel_border)
            .child(match conversation {
                Some(c) => conversation_avatar(c, px(32.)),
                None => avatar(&title, color, px(32.)).into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                    .when_some(subtitle, |d, s| {
                        d.child(div().text_xs().text_color(palette.text_secondary).child(s))
                    }),
            )
            .child(div().flex_1())
            .when(timer > 0, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(palette.text_secondary)
                        .child(Icon::new(IconName::Timer).size_4())
                        .child(format_timer(timer)),
                )
            })
            .when(conversation.is_some(), |d| {
                use gpui_kit::component::button::ButtonVariants as _;
                d.child(
                    gpui_kit::component::button::Button::new("chat-search")
                        .ghost()
                        .icon(IconName::Search)
                        .tooltip("Search in chat")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let chat = this.id.clone();
                            this.state.update(cx, |state, cx| state.search_in_chat(chat, cx));
                        })),
                )
                .child(self.render_menu_button(cx))
            })
    }

    /// Desktop's "⋯" chat menu.
    fn render_menu_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::DropdownMenu as _;
        let (state, id) = (self.state.clone(), self.id.clone());
        let view = cx.entity().downgrade();
        let header = crate::views::chat_menu::HeaderActions {
            select_messages: Some(std::rc::Rc::new(move |_, cx| {
                let _ = view.update(cx, |this, cx| this.start_selecting(None, cx));
            })),
        };
        Button::new("chat-menu")
            .ghost()
            .icon(IconName::Ellipsis)
            .tooltip("More options")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                match state.read(cx).conversation(&id).cloned() {
                    Some(conversation) => crate::views::chat_menu::build(menu, &conversation, &state, header.clone(), window, cx),
                    None => menu,
                }
            })
    }

    /// In place of the composer when the chat is blocked, as Desktop shows.
    fn render_blocked(&self, conversation: &signal_store::Conversation, palette: Palette) -> impl IntoElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        let group = conversation.kind == ConversationKind::Group;
        let text = if group {
            "Unblock this group and share your name and photo with its members? You won't receive any messages until you unblock them.".to_owned()
        } else {
            format!(
                "Let {} message you and share your name and photo with them? You won't receive any messages until you unblock them.",
                conversation.title
            )
        };
        let (state, id, title) = (self.state.clone(), self.id.clone(), conversation.title.clone());
        let (delete_state, delete_id) = (self.state.clone(), self.id.clone());
        div()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .px_6()
            .py_4()
            .border_t_1()
            .border_color(palette.panel_border)
            .child(div().text_sm().text_color(palette.text_secondary).text_center().child(text))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("blocked-delete").label("Delete").outline().on_click(move |_, window, cx| {
                        crate::views::chat_menu::confirm_delete_chat(delete_state.clone(), delete_id.clone(), group, window, cx)
                    }))
                    .child(Button::new("blocked-unblock").label("Unblock").primary().on_click(move |_, window, cx| {
                        crate::views::chat_menu::confirm_block(state.clone(), id.clone(), &title, group, false, window, cx)
                    })),
            )
    }

    fn render_item(&mut self, ix: usize, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(message) = self.messages.get(ix).cloned() else {
            return div().into_any_element();
        };
        let prev = ix.checked_sub(1).and_then(|i| self.messages.get(i));
        let next = self.messages.get(ix + 1);
        let is_group = self.state.read(cx).conversation(&self.id).is_some_and(|c| c.kind == ConversationKind::Group);

        let show_day = prev.is_none_or(|p| !same_day(p.sent_at, message.sent_at));
        if let Some(notice) = &message.notice {
            return self.render_notice(notice, show_day, message.sent_at, palette, cx);
        }
        let continues = |a: &Message, b: &Message| {
            a.notice.is_none()
                && b.notice.is_none()
                && a.direction == b.direction
                && a.author == b.author
                && (b.sent_at - a.sent_at).abs() < GROUP_WINDOW_MS
                && same_day(a.sent_at, b.sent_at)
        };
        let first_in_group = show_day || prev.is_none_or(|p| !continues(p, &message));
        let last_in_group = next.is_none_or(|n| !continues(&message, n));
        let outgoing = message.direction == Direction::Outgoing;
        let author_name = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
        let author_color = color_for(message.author.as_deref().unwrap_or(""));
        let author_photo = message.author.as_deref().and_then(|a| self.state.read(cx).photo_of(a)).map(|p| p.to_path_buf());

        let bubble = self.render_bubble(&message, is_group && first_in_group, first_in_group, last_in_group, palette, cx);

        let hovered = self.hovered == Some(message.id) || self.menu_open_for == Some(message.id);
        let selecting = self.selecting.as_ref().map(|s| s.contains(&message.id));
        let actions = if hovered && !message.deleted && selecting.is_none() {
            Some(self.render_hover_actions(&message, palette, cx).into_any_element())
        } else {
            None
        };
        let reaction_bar = if self.reaction_bar_for == Some(message.id) {
            Some(self.render_reaction_bar(&message, palette, cx).into_any_element())
        } else {
            None
        };
        let reactions = if message.reactions.is_empty() {
            None
        } else {
            Some(self.render_reactions(&message, palette, cx).into_any_element())
        };
        let message_id = message.id;

        let row = div()
            .id(("message", message.id as u64))
            .debug_selector(move || format!("row-{message_id}"))
            .relative()
            .flex()
            .w_full()
            .px_4()
            // Select mode: a checkbox on the left; a click picks the message.
            .when_some(selecting, |d, on| {
                d.pl(px(52.)).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.toggle_selected(message_id, cx))).child(
                    div()
                        .absolute()
                        .left_4()
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .debug_selector(move || format!("select-{message_id}"))
                                .size(px(20.))
                                .rounded_full()
                                .border_2()
                                .border_color(if on { palette.accent } else { palette.text_secondary })
                                .when(on, |d| d.bg(palette.accent)),
                        ),
                )
            })
            // A search result just jumped to.
            .when(self.highlight == Some(message_id), |d| d.bg(palette.row_hover))
            .when(first_in_group, |d| d.pt_2())
            .when(!first_in_group, |d| d.pt(px(2.)))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hovered = Some(message_id);
                } else if this.hovered == Some(message_id) {
                    this.hovered = None;
                }
                cx.notify();
            }))
            .when(outgoing, |d| d.justify_end())
            .when(!outgoing && is_group, |d| {
                d.child(
                    div()
                        .w(px(36.))
                        .flex_none()
                        .flex()
                        .items_end()
                        // Level with the bubble's bottom, not the reactions under it.
                        .pb(px(reactions_below(&message)))
                        .when(last_in_group, |d| {
                            d.child(
                                div()
                                    .debug_selector(move || format!("avatar-{message_id}"))
                                    .child(person_avatar(author_photo.as_deref(), &author_name, author_color, px(28.))),
                            )
                        }),
                )
            })
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .max_w(gpui_kit::relative(0.75))
                    .when(outgoing, |d| d.flex_row_reverse())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .when(outgoing, |d| d.items_end())
                            .child(bubble)
                            .when_some(reactions, |d, r| d.child(r)),
                    )
                    // Positioned outside the bubble so showing them doesn't
                    // take width from it and re-wrap the text.
                    .when_some(actions, |d, a| {
                        d.child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .when(outgoing, |d| d.right_full().pr_2())
                                .when(!outgoing, |d| d.left_full().pl_2())
                                .child(a),
                        )
                    })
                    .when_some(reaction_bar, |d, bar| d.child(deferred(bar).with_priority(1))),
            );

        // Right-click: the same menu as "⋯" (not in select mode).
        let row = if self.selecting.is_none() {
            use gpui_kit::component::menu::ContextMenuExt as _;
            let (menu_message, view) = (message.clone(), cx.entity().downgrade());
            row.context_menu(move |menu, _, cx| message_menu(menu, &menu_message, view.clone(), cx)).into_any_element()
        } else {
            row.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .when(ix == 0 && self.has_more, |d| {
                d.child(
                    div()
                        .py_2()
                        .flex()
                        .justify_center()
                        .text_xs()
                        .text_color(palette.text_secondary)
                        .child("Loading…"),
                )
            })
            .when(show_day, |d| {
                d.child(
                    div()
                        .pt_4()
                        .pb_2()
                        .flex()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.text_secondary)
                        .child(day_label(message.sent_at)),
                )
            })
            .child(row)
            .when(ix + 1 == self.messages.len(), |d| d.child(div().h_3()))
            .into_any_element()
    }

    /// `first`/`last`: the message's place in a run from the same sender,
    /// which shapes its corners; only the last shows the time.
    fn render_bubble(
        &self,
        message: &Message,
        show_author: bool,
        first: bool,
        last: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_meta = last;
        let outgoing = message.direction == Direction::Outgoing;
        let corners = Corners::for_bubble(outgoing, first, last);
        let (bg, fg, meta_color) = if outgoing {
            (palette.bubble_out, palette.bubble_out_text, palette.bubble_out_meta)
        } else {
            (palette.bubble_in, palette.bubble_in_text, palette.text_secondary)
        };

        let jumbo = jumbo_count(message);

        let meta = show_meta.then(|| {
            let color = if jumbo.is_some() { palette.text_secondary } else { meta_color };
            render_meta(message, color, palette)
        });

        if let Some(count) = jumbo {
            let size = match count {
                1 => 56.,
                2 => 48.,
                3 => 40.,
                _ => 34.,
            };
            return div()
                .flex()
                .flex_col()
                .when(outgoing, |d| d.items_end())
                .child(div().text_size(px(size)).line_height(px(size * 1.2)).child(message.body.clone()))
                .when_some(meta, |d, m| d.child(m))
                .into_any_element();
        }

        // Stickers stand alone, without a bubble, at Desktop's size.
        if let Some(sticker) = message.attachments.first().filter(|a| a.sticker && !message.deleted) {
            let meta = show_meta.then(|| render_meta(message, palette.text_secondary, palette));
            let (w, h) = match (sticker.width, sticker.height) {
                (Some(w), Some(h)) if w > 0 && h > 0 => {
                    let scale = (STICKER_SIZE / w as f32).min(STICKER_SIZE / h as f32);
                    (w as f32 * scale, h as f32 * scale)
                }
                _ => (STICKER_SIZE, STICKER_SIZE),
            };
            return div()
                .debug_selector(|| format!("bubble-{}", message.id))
                .flex()
                .flex_col()
                .when(outgoing, |d| d.items_end())
                .child(match &sticker.local_path {
                    Some(path) => img(std::path::PathBuf::from(path))
                        .w(px(w))
                        .h(px(h))
                        .object_fit(gpui_kit::ObjectFit::Contain)
                        .into_any_element(),
                    // Not downloaded yet.
                    None => div().size(px(STICKER_SIZE)).rounded_lg().bg(palette.bubble_in).into_any_element(),
                })
                .when_some(meta, |d, m| d.child(m))
                .into_any_element();
        }

        let author_name = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
        let author_color = {
            let members = self.state.read(cx).conversation(&self.id).map(|c| c.members.clone()).unwrap_or_default();
            crate::theme::name_color(&members, message.author.as_deref().unwrap_or(""), gpui_kit::component::ActiveTheme::theme(&**cx).is_dark())
        };
        let author_label = message
            .author
            .as_deref()
            .and_then(|author| self.state.read(cx).conversation(&self.id)?.member_labels.get(author).cloned());

        // Like Signal Desktop, a downloaded photo fills the bubble edge to edge
        // and sets its width; a caption wraps below it.
        let photo = (!message.deleted)
            .then(|| message.attachments.first())
            .flatten()
            .filter(|a| a.content_type.starts_with("image/") || a.gif)
            .and_then(|a| Some((a.local_path.clone()?, media_size(a.width, a.height), viewed_image(a), a.gif)));
        if let Some((path, (width, height), viewed, gif)) = photo {
            let header = (show_author && !outgoing) || message.quote.is_some();
            let caption = !message.body.is_empty();
            let rest = &message.attachments[1..];
            let footer = caption || !rest.is_empty();
            // A GIF is a looping video, decoded to frames (at 2× for Retina);
            // a plain box shows until they're ready.
            let source: Option<gpui_kit::ImageSource> = if gif {
                let path = std::path::PathBuf::from(&path);
                self.state
                    .update(cx, |state, cx| state.video_loop(&path, (width * 2.) as u32, cx))
                    .map(Into::into)
            } else {
                Some(std::path::PathBuf::from(path).into())
            };
            // Corners the photo shares with the bubble get the bubble's radius.
            let image = match source {
                Some(source) => img(source).w_full().h(px(height)).object_fit(gpui_kit::ObjectFit::Cover).into_any_element(),
                None => div().w_full().h(px(height)).bg(palette.row_hover).into_any_element(),
            };
            let image = div()
                .w_full()
                .h(px(height))
                .overflow_hidden()
                .when(!header, |d| d.rounded_tl(px(corners.top_left)).rounded_tr(px(corners.top_right)))
                .when(!footer, |d| d.rounded_bl(px(corners.bottom_left)).rounded_br(px(corners.bottom_right)))
                .child(image);
            return corners
                .apply(div())
                .debug_selector(|| format!("bubble-{}", message.id))
                .flex()
                .flex_col()
                .w(px(width))
                .bg(bg)
                .text_color(fg)
                .when(header, |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .pt_2()
                            .pb_1p5()
                            .when(show_author && !outgoing, |d| {
                                d.child(author_line(author_name, author_color, author_label))
                            })
                            .when_some(message.quote.clone(), |d, quote| d.child(render_quote(quote, fg, palette))),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("photo-{}", message.id)))
                        .relative()
                        // Photos open in the viewer; GIFs just play.
                        .when_some(viewed.filter(|_| !gif), |d, viewed| {
                            d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                let image = viewed.clone();
                                this.state.update(cx, |state, cx| state.view_image(image, cx));
                            }))
                        })
                        .child(image)
                        // No caption: the time sits on the photo, as in Desktop.
                        .when(!footer && show_meta, |d| {
                            d.child(
                                div()
                                    .absolute()
                                    .bottom_2()
                                    .right_2()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(Hsla { h: 0., s: 0., l: 0., a: 0.35 })
                                    .child(render_meta(message, palette.on_accent, palette)),
                            )
                        }),
                )
                .when(footer, |d| {
                    let inline_meta = caption && meta.is_some();
                    let spacer = inline_meta.then(|| meta_spacer(message, cx));
                    d.child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .pt_1p5()
                            .pb_2()
                            .children(rest.iter().enumerate().map(|(ix, a)| {
                                render_attachment(message.id, ix + 1, a, fg, palette, cx)
                            }))
                            .when(caption, |d| d.child(message_text(message.id, &message.body, &message.mentions, spacer.as_deref().map(|s| (s, bg)))))
                            .when_some(meta, |d, m| d.child(place_meta(m, inline_meta))),
                    )
                })
                .into_any_element();
        }

        // A link preview sits edge to edge between the header and the text.
        if let Some(preview) =
            message.link_preview.as_ref().filter(|_| !message.deleted && message.attachments.is_empty())
        {
            let header = (show_author && !outgoing) || message.quote.is_some();
            let full = full_size_preview_image(preview);
            let inline_meta = meta.is_some() && !message.body.is_empty();
            let spacer = inline_meta.then(|| meta_spacer(message, cx));
            return corners
                .apply(div())
                .debug_selector(|| format!("bubble-{}", message.id))
                .flex()
                .flex_col()
                .when_some(full.as_ref().map(|(_, (width, _))| *width), |d, width| d.w(px(width)))
                .bg(bg)
                .text_color(fg)
                .when(header, |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .pt_2()
                            .pb_1p5()
                            .when(show_author && !outgoing, |d| {
                                d.child(author_line(author_name, author_color, author_label))
                            })
                            .when_some(message.quote.clone(), |d, quote| d.child(render_quote(quote, fg, palette))),
                    )
                })
                .child(render_preview_card(message.id, preview, full, (!header).then_some(corners), meta_color))
                .child(
                    div()
                        .relative()
                        .px_3()
                        .pt_1p5()
                        .pb_2()
                        .when(!message.body.is_empty(), |d| {
                            d.child(message_text(message.id, &message.body, &message.mentions, spacer.as_deref().map(|s| (s, bg))))
                        })
                        .when_some(meta, |d, m| d.child(place_meta(m, inline_meta))),
                )
                .into_any_element();
        }

        // The time shares the last line of text when it fits, as in Desktop.
        let inline_meta = meta.is_some() && !message.deleted && !message.body.is_empty();
        let spacer = inline_meta.then(|| meta_spacer(message, cx));
        corners
            .apply(div())
            .debug_selector(|| format!("bubble-{}", message.id))
            .relative()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(bg)
            .text_color(fg)
            .when(show_author && !outgoing, |d| {
                d.child(author_line(author_name, author_color, author_label))
            })
            .when_some(message.quote.clone(), |d, quote| d.child(render_quote(quote, fg, palette)))
            .children(message.attachments.iter().enumerate().map(|(ix, a)| render_attachment(message.id, ix, a, fg, palette, cx)))
            .map(|d| {
                if message.deleted {
                    d.child(div().italic().child(if outgoing {
                        "You deleted this message."
                    } else {
                        "This message was deleted."
                    }))
                } else if !message.body.is_empty() {
                    d.child(message_text(message.id, &message.body, &message.mentions, spacer.as_deref().map(|s| (s, bg))))
                } else {
                    d
                }
            })
            .when_some(meta, |d, m| d.child(place_meta(m, inline_meta)))
            .into_any_element()
    }

    fn render_hover_actions(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let id = message.id;
        let for_reply = message.clone();
        let downloadable: Vec<AttachmentMeta> =
            message.attachments.iter().filter(|a| a.local_path.is_some() && !a.sticker).cloned().collect();
        let button = |name: &'static str, icon: IconName| {
            div()
                .id(SharedString::from(format!("{name}-{id}")))
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .child(Icon::new(icon).size_4().text_color(palette.text_secondary))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .child(button("react", IconName::FaceSlightlySmilingPlus).on_click(cx.listener(move |this, _, _, cx| {
                this.reaction_bar_for = if this.reaction_bar_for == Some(id) { None } else { Some(id) };
                cx.notify();
            })))
            .when(!downloadable.is_empty(), |d| {
                d.child(button("download", IconName::Download).on_click(move |_, _, cx: &mut App| {
                    download_attachments(downloadable.clone(), cx);
                }))
            })
            .child(button("reply", IconName::Reply).on_click(cx.listener(move |this, _, window, cx| {
                let message = for_reply.clone();
                this.composer.update(cx, |composer, cx| composer.reply_to(message, window, cx));
            })))
            .child(self.render_message_menu_button(message, cx))
    }

    /// The "⋯" button and its menu; the hover buttons stay while it's open.
    fn render_message_menu_button(&self, message: &Message, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::DropdownMenu as _;
        let id = message.id;
        let message = message.clone();
        let view = cx.entity().downgrade();
        let opened = cx.entity().downgrade();
        Button::new(SharedString::from(format!("menu-{id}")))
            .ghost()
            .small()
            .icon(IconName::Ellipsis)
            .dropdown_menu(move |menu, _, cx| message_menu(menu, &message, view.clone(), cx))
            .on_open_change(move |open, _, cx| {
                let _ = opened.update(cx, |this, cx| {
                    this.menu_open_for = if *open { Some(id) } else { None };
                    cx.notify();
                });
            })
    }

    /// Enters select mode with `first` chosen (Desktop's "Select").
    fn start_selecting(&mut self, first: Option<i64>, cx: &mut Context<Self>) {
        self.selecting = Some(first.into_iter().collect());
        self.hovered = None;
        self.reaction_bar_for = None;
        cx.notify();
    }

    fn toggle_selected(&mut self, id: i64, cx: &mut Context<Self>) {
        if let Some(selected) = &mut self.selecting
            && !selected.remove(&id)
        {
            selected.insert(id);
        }
        cx.notify();
    }

    /// Desktop's select mode bar, in place of the composer.
    fn render_select_bar(&self, selected: &std::collections::BTreeSet<i64>, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::Disableable as _;
        let count = selected.len();
        let ids: Vec<i64> = selected.iter().copied().collect();
        let chosen: Vec<Message> = self.messages.iter().filter(|m| selected.contains(&m.id)).cloned().collect();
        let note_to_self = self.note_to_self(cx);
        let now = now_millis();
        let for_everyone = !chosen.is_empty()
            && chosen.len() <= signal_backend::deletes::MAX_FOR_EVERYONE
            && chosen.iter().all(|m| signal_backend::deletes::can_send(m, note_to_self, now));
        let forwardable = !chosen.is_empty()
            && chosen.len() <= crate::views::forward::MAX_MESSAGES
            && chosen.iter().all(|m| !m.deleted);
        let (forward_ids, delete_ids) = (ids.clone(), ids);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(palette.panel_border)
            .child(
                Button::new("select-exit")
                    .ghost()
                    .icon(IconName::X)
                    .tooltip("Exit select mode")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selecting = None;
                        cx.notify();
                    })),
            )
            .child(div().flex_1().child(format!("{count} selected")))
            .child(
                Button::new("select-forward")
                    .ghost()
                    .icon(IconName::Forward)
                    .tooltip("Forward selected messages")
                    .disabled(!forwardable)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selecting = None;
                        crate::views::forward::open(this.state.clone(), forward_ids.clone(), window, cx);
                    })),
            )
            .child(
                Button::new("select-delete")
                    .ghost()
                    .icon(IconName::Trash)
                    .tooltip("Delete selected messages")
                    .disabled(count == 0)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let ids = delete_ids.clone();
                        confirm_delete(this.state.clone(), ids, note_to_self, for_everyone, window, cx);
                        this.selecting = None;
                        cx.notify();
                    })),
            )
    }

    fn note_to_self(&self, cx: &App) -> bool {
        self.state.read(cx).account.as_ref().is_some_and(|a| self.id.as_direct() == Some(a.aci.as_str()))
    }

    fn render_reaction_bar(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let id = message.id;
        let mine = message.reactions.iter().find(|r| r.author.is_none()).map(|r| r.emoji.clone());
        let tone = self.picker.read(cx).skin_tone();
        let outgoing = message.direction == Direction::Outgoing;
        div()
            .absolute()
            .bottom_full()
            .when(outgoing, |d| d.right_0())
            .when(!outgoing, |d| d.left_0())
            .mb_1()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_full()
            .bg(palette.popover)
            .border_1()
            .border_color(palette.panel_border)
            .shadow_lg()
            .occlude()
            .children(QUICK_REACTIONS.iter().enumerate().map(|(ix, base)| {
                let glyph = signal_emoji::by_glyph(base)
                    .map_or(*base, |(e, _)| e.glyph_with(tone))
                    .to_owned();
                let selected = mine.as_deref() == Some(glyph.as_str());
                let emoji = glyph.clone();
                div()
                    .id(("quick-reaction", ix))
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .text_size(px(22.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(palette.reaction_chip_mine))
                    .hover(|d| d.bg(palette.row_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.react(id, emoji.clone(), cx)))
                    .child(glyph)
            }))
            .child(
                div()
                    .id("more-reactions")
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .hover(|d| d.bg(palette.row_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reaction_bar_for = None;
                        this.reaction_picker_for = Some(id);
                        // The composer shares the picker; it would take the pick too.
                        this.composer.update(cx, |composer, cx| composer.close_picker(window, cx));
                        this.picker.update(cx, |picker, cx| {
                            picker.reset(window, cx);
                            picker.set_media(false, window, cx);
                        });
                        cx.notify();
                    }))
                    .child(Icon::new(IconName::Plus).size_4().text_color(palette.text_secondary)),
            )
    }

    /// Reaction pills on the bubble's inside bottom corner (bottom right for
    /// incoming, bottom left for outgoing), overlapping its edge as in Signal
    /// Desktop. Past three kinds, the top two show plus "+N". Clicking any of
    /// them opens the reaction viewer.
    fn render_reactions(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = reaction_groups(&message.reactions);
        let id = message.id;
        let outgoing = message.direction == Direction::Outgoing;
        let overlap = reactions_overlap(message);
        let shown = if groups.len() > MAX_REACTION_PILLS { MAX_REACTION_PILLS - 1 } else { groups.len() };
        let hidden = groups.len() - shown;
        let viewer = self
            .reaction_viewer
            .as_ref()
            .filter(|(open, _)| *open == id)
            .map(|(_, tab)| self.render_reaction_viewer(message, tab.clone(), palette, cx));
        let open = |emoji: Option<String>| {
            cx.listener(move |this: &mut Self, _: &gpui_kit::ClickEvent, _, cx| {
                this.reaction_viewer = match &this.reaction_viewer {
                    Some((open, _)) if *open == id => None,
                    _ => Some((id, emoji.clone())),
                };
                cx.notify();
            })
        };
        let pill = move |key: String, mine: bool| {
            div()
                .id(SharedString::from(key))
                .flex()
                .items_center()
                .gap_1()
                .px_1p5()
                .h(px(24.))
                .rounded_full()
                .border_2()
                .border_color(palette.background)
                .bg(if mine { palette.reaction_chip_mine } else { palette.reaction_chip })
                .cursor_pointer()
        };
        div()
            .debug_selector(|| format!("reactions-{id}"))
            .relative()
            .flex()
            .gap_1()
            .when(overlap, |d| d.mt(px(-REACTION_OVERLAP)))
            .when(!overlap, |d| d.mt(px(REACTION_GAP)))
            .mx_3()
            .when(outgoing, |d| d.self_start())
            .when(!outgoing, |d| d.self_end())
            .children(groups.iter().take(shown).enumerate().map(|(ix, group)| {
                pill(format!("reaction-{id}-{ix}"), group.mine)
                    .on_click(open(Some(group.emoji.clone())))
                    .child(div().text_size(px(14.)).child(group.emoji.clone()))
                    .when(group.count > 1, |d| {
                        d.child(div().text_xs().text_color(palette.text).child(group.count.to_string()))
                    })
            }))
            .when(hidden > 0, |d| {
                d.child(
                    pill(format!("reaction-{id}-more"), false)
                        .debug_selector(|| format!("reaction-{id}-more"))
                        .on_click(open(None))
                        .child(div().text_xs().text_color(palette.text).child(format!("+{hidden}"))),
                )
            })
            .when_some(viewer, |d, viewer| {
                // Above the pills, anchored at their outer corner; gpui flips
                // it below (or to the other side) when it wouldn't fit.
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .when(outgoing, |d| d.left_0())
                        .when(!outgoing, |d| d.right_0())
                        .child(deferred(
                            anchored()
                                .anchor(if outgoing { Anchor::BottomLeft } else { Anchor::BottomRight })
                                .child(div().mb_2().child(viewer)),
                        )),
                )
            })
    }

    /// Who reacted with what (Signal Desktop's reaction viewer): a tab per
    /// emoji plus All, then one row per person. Your own row removes your
    /// reaction.
    fn render_reaction_viewer(
        &self,
        message: &Message,
        tab: Option<String>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = message.id;
        let groups = reaction_groups(&message.reactions);
        let mut reactions: Vec<&Reaction> =
            message.reactions.iter().filter(|r| tab.as_ref().is_none_or(|t| *t == r.emoji)).collect();
        // Most recent first, like Desktop.
        reactions.sort_by_key(|r| std::cmp::Reverse(r.sent_at));
        let tab_button = |key: String, selected: bool, label: String, emoji: Option<String>| {
            div()
                .debug_selector(|| key.clone())
                .id(SharedString::from(key))
                .flex()
                .items_center()
                .h(px(32.))
                .px_3()
                .rounded_full()
                .cursor_pointer()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .when(selected, |d| d.bg(palette.reaction_chip_mine))
                .when(!selected, |d| d.hover(|d| d.bg(palette.row_hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.reaction_viewer = Some((id, emoji.clone()));
                    cx.notify();
                }))
                .child(label)
        };
        div()
            .id(SharedString::from(format!("reaction-viewer-{id}")))
            .debug_selector(|| format!("reaction-viewer-{id}"))
            // Clicks land here, not on the message or pills underneath.
            .occlude()
            .w(px(320.))
            .max_h(px(320.))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(palette.popover)
            .border_1()
            .border_color(palette.panel_border)
            .shadow_lg()
            .text_color(palette.text)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.reaction_viewer = None;
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .p_2()
                    .child(tab_button(
                        format!("reaction-tab-{id}-all"),
                        tab.is_none(),
                        format!("All · {}", message.reactions.len()),
                        None,
                    ))
                    .children(groups.iter().enumerate().map(|(ix, group)| {
                        tab_button(
                            format!("reaction-tab-{id}-{ix}"),
                            tab.as_ref() == Some(&group.emoji),
                            format!("{} {}", group.emoji, group.count),
                            Some(group.emoji.clone()),
                        )
                    })),
            )
            .child(
                div()
                    .id(SharedString::from(format!("reaction-list-{id}")))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .pb_2()
                    .children(reactions.into_iter().enumerate().map(|(ix, reaction)| {
                        let mine = reaction.author.is_none();
                        let name = self.reactor_name(reaction.author.as_deref(), cx);
                        let color = color_for(reaction.author.as_deref().unwrap_or(""));
                        let emoji = reaction.emoji.clone();
                        div()
                            .id(SharedString::from(format!("reaction-row-{id}-{ix}")))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_1p5()
                            .child(person_avatar(
                                reaction.author.as_deref().and_then(|a| self.state.read(cx).photo_of(a)),
                                &name,
                                color,
                                px(32.),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(name))
                                    .when(mine, |d| {
                                        d.child(div().text_xs().text_color(palette.text_secondary).child("Click to remove"))
                                    }),
                            )
                            .child(div().text_size(px(20.)).child(reaction.emoji.clone()))
                            .when(mine, |d| {
                                d.cursor_pointer().hover(|d| d.bg(palette.row_hover)).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.reaction_viewer = None;
                                        this.react(id, emoji.clone(), cx);
                                    },
                                ))
                            })
                    })),
            )
            .into_any_element()
    }

    /// A reactor's name from what this view already knows: their direct
    /// conversation's title, or a name they posted under here.
    fn reactor_name(&self, author: Option<&str>, cx: &App) -> String {
        let Some(author) = author else { return "You".into() };
        let state = self.state.read(cx);
        state
            .conversation(&ConversationId::direct(author))
            .map(|c| c.title.clone())
            .or_else(|| {
                self.messages.iter().rev().find(|m| m.author.as_deref() == Some(author)).and_then(|m| m.author_name.clone())
            })
            .unwrap_or_else(|| author.get(..8).unwrap_or(author).to_owned())
    }

    /// Signal Desktop's typing bubble: three pulsing dots in an incoming
    /// bubble; in groups, the typists' avatars beside it (up to three,
    /// overlapping, then "+N").
    fn render_typing(&self, palette: Palette, cx: &App) -> Option<impl IntoElement + use<>> {
        use gpui_kit::{Animation, AnimationExt as _};
        let state = self.state.read(cx);
        let typists = state.typing_in(&self.id);
        if typists.is_empty() {
            return None;
        }
        let is_group = state.conversation(&self.id).is_some_and(|c| c.kind == ConversationKind::Group);
        let overflow = typists.len().saturating_sub(MAX_TYPING_AVATARS);
        // The three who started typing last, newest first (leftmost).
        let shown: Vec<&String> = typists.iter().rev().take(MAX_TYPING_AVATARS).collect();
        let dot = |ix: usize| {
            div().size(px(8.)).flex().items_center().justify_center().child(
                div().size(px(6.)).rounded_full().bg(palette.bubble_in_text).with_animation(
                    ("typing-dot", ix),
                    Animation::new(std::time::Duration::from_millis(1600)).repeat(),
                    move |dot, t| {
                        let (opacity, scale) = typing_dot(t, ix);
                        dot.opacity(opacity).size(px(6. * scale))
                    },
                ),
            )
        };
        Some(
            div()
                .debug_selector(|| "typing-bubble".into())
                .px_4()
                .pt_2()
                .pb_1()
                .flex()
                .items_end()
                .when(is_group, |d| {
                    // Desktop's stack: newest typist leftmost (where message
                    // avatars sit) and on top, each next one 4pt under the
                    // one before, then "+N"; each 28pt avatar has a 3pt ring
                    // that takes no room, like Desktop's outline. More
                    // typists push the bubble right.
                    let ring = |content: AnyElement| {
                        div()
                            .relative()
                            .flex_none()
                            .size(px(28.))
                            .child(div().absolute().top(px(-3.)).left(px(-3.)).size(px(34.)).rounded_full().bg(palette.background))
                            .child(content)
                    };
                    let mut stack: Vec<AnyElement> = Vec::new();
                    if overflow > 0 {
                        stack.push(
                            div()
                                .size(px(28.))
                                .rounded_full()
                                .bg(palette.bubble_in)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(palette.text_secondary)
                                .child(format!("+{overflow}"))
                                .into_any_element(),
                        );
                    }
                    // Drawn right to left, so the leftmost ends up on top.
                    for (ix, aci) in shown.iter().enumerate().rev() {
                        stack.push(
                            div()
                                .debug_selector(move || format!("typing-avatar-{ix}"))
                                .child(person_avatar(state.photo_of(aci), &self.reactor_name(Some(aci), cx), color_for(aci), px(28.)))
                                .into_any_element(),
                        );
                    }
                    // `stack` runs right to left; place each 24pt apart (28pt
                    // overlapped by 4) in a box as wide as all of them.
                    let count = stack.len();
                    let width = 28. + 24. * (count - 1) as f32;
                    d.child(
                        div()
                            .debug_selector(|| "typing-avatars".into())
                            .relative()
                            .flex_none()
                            .w(px(width))
                            .h(px(28.))
                            .mr_2()
                            .children(stack.into_iter().enumerate().map(|(ix, item)| {
                                let from_left = count - 1 - ix;
                                ring(item).absolute().top_0().left(px(24. * from_left as f32))
                            })),
                    )
                })
                .child(
                    div()
                        .debug_selector(|| "typing-dots".into())
                        .h(px(36.))
                        .px_3()
                        .flex()
                        .items_center()
                        .gap(px(3.))
                        .rounded(px(BUBBLE_RADIUS))
                        .bg(palette.bubble_in)
                        .children((0..3).map(dot)),
                ),
        )
    }
}

fn render_meta(message: &Message, color: Hsla, palette: Palette) -> impl IntoElement {
    let outgoing = message.direction == Direction::Outgoing;
    let status = outgoing
        .then(|| status_icon(message.status, color, palette, StatusPlace::Bubble, message.id.to_string()))
        .flatten();
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(META_GAP))
        .text_size(px(META_SIZE))
        .text_color(color)
        .when(message.edited, |d| d.child("Edited ·"))
        .when(message.expires_in > 0, |d| d.child(Icon::new(IconName::Timer).size_3().text_color(color)))
        .when(message.status == DeliveryStatus::Failed && outgoing, |d| {
            d.child(div().text_color(palette.danger).child("Not sent"))
        })
        .child(message_time(message.sent_at))
        // Desktop leaves 6pt before the status icon.
        .when_some(status, |d, s| d.child(div().ml(px(STATUS_GAP)).child(s)))
}

const BUBBLE_RADIUS: f32 = 18.;
/// Signal Desktop draws stickers up to 200pt.
const STICKER_SIZE: f32 = 200.;

/// A downloaded preview image big enough to span the bubble (Desktop's
/// `shouldUseFullSizeLinkPreviewImage`: both sides ≥ 200 and not roughly
/// square), with its path and display size.
fn full_size_preview_image(preview: &signal_store::LinkPreview) -> Option<(String, (f32, f32))> {
    let image = preview.image.as_ref()?;
    let (width, height) = (image.width?, image.height?);
    let roughly_square = (1. - width as f32 / height.max(1) as f32).abs() < 0.05;
    (width >= 200 && height >= 200 && !roughly_square && image.content_type.starts_with("image/"))
        .then(|| Some((image.local_path.clone()?, media_size(Some(width), Some(height)))))
        .flatten()
}

/// Side of a small preview image (Desktop: 72).
const PREVIEW_THUMBNAIL: f32 = 72.;

/// The link preview card: a full-width image, or a thumbnail beside the
/// title, description and "domain · date". Opens the link when clicked.
/// `top_corners` rounds a full-width image that starts the bubble.
fn render_preview_card(
    message_id: i64,
    preview: &signal_store::LinkPreview,
    full: Option<(String, (f32, f32))>,
    top_corners: Option<Corners>,
    secondary: Hsla,
) -> impl IntoElement {
    let url = preview.url.clone();
    let domain = url::Url::parse(&preview.url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default();
    let date = preview
        .date
        .and_then(|d| chrono::TimeZone::timestamp_millis_opt(&chrono::Local, d).single())
        .map(|d| d.format("%b %-d, %Y").to_string());
    let footer = match date {
        Some(date) => format!("{domain} · {date}"),
        None => domain,
    };
    let thumbnail = full
        .is_none()
        .then(|| preview.image.as_ref()?.local_path.clone())
        .flatten();
    div()
        .id(SharedString::from(format!("link-preview-{message_id}")))
        .flex()
        .flex_col()
        .bg(Hsla { h: 0., s: 0., l: 0., a: 0.14 })
        .cursor_pointer()
        .on_click(move |_, _, cx| cx.open_url(&url))
        .when_some(full, |d, (path, (_, height))| {
            d.child(
                img(std::path::PathBuf::from(path))
                    .w_full()
                    .h(px(height))
                    .object_fit(gpui_kit::ObjectFit::Cover)
                    .when_some(top_corners, |d, c| d.rounded_tl(px(c.top_left)).rounded_tr(px(c.top_right))),
            )
        })
        .child(
            div()
                .relative()
                .flex()
                .flex_col()
                .gap(px(2.))
                .px_3()
                .py_2()
                .when(thumbnail.is_some(), |d| {
                    d.pl(px(12. + PREVIEW_THUMBNAIL + 8.)).min_h(px(PREVIEW_THUMBNAIL + 16.))
                })
                .when_some(thumbnail, |d, path| {
                    d.child(
                        img(std::path::PathBuf::from(path))
                            .absolute()
                            .left_3()
                            .top_2()
                            .size(px(PREVIEW_THUMBNAIL))
                            .rounded_md()
                            .object_fit(gpui_kit::ObjectFit::Cover),
                    )
                })
                .when_some(preview.title.clone(), |d, title| {
                    d.child(div().font_weight(FontWeight::SEMIBOLD).line_clamp(2).text_ellipsis().child(title))
                })
                .when_some(preview.description.clone(), |d, description| {
                    d.child(div().text_sm().line_clamp(3).text_ellipsis().child(description))
                })
                .child(div().text_sm().text_color(secondary).truncate().child(footer)),
        )
}

/// Emoji-only messages (up to five) render large, without a bubble.
fn jumbo_count(message: &Message) -> Option<usize> {
    (!message.deleted && message.quote.is_none() && message.attachments.is_empty())
        .then(|| signal_emoji::jumbo_count(&message.body))
        .flatten()
}

fn is_jumbo(message: &Message) -> bool {
    jumbo_count(message).is_some()
}
/// Typists shown beside the typing bubble before "+N" (Desktop's limit).
const MAX_TYPING_AVATARS: usize = 3;

/// One dot of the typing animation at `t` (0–1 over 1.6s), staggered by
/// 160ms per dot: Desktop's keyframes, 40% opacity resting, full and 1.3×
/// at its peak.
/// Reaction pills: 24pt tall, overlapping the bubble's bottom edge by 8pt or
/// sitting 4pt under it.
const REACTION_PILL: f32 = 24.;
const REACTION_OVERLAP: f32 = 8.;
const REACTION_GAP: f32 = 4.;

/// Jumbo emoji have no bubble edge to sit on (and the time is right below
/// them), so their pills go underneath; so do stickers'.
fn reactions_overlap(message: &Message) -> bool {
    !is_jumbo(message) && !message.attachments.first().is_some_and(|a| a.sticker)
}

/// How far a message's reaction pills reach below its bubble.
fn reactions_below(message: &Message) -> f32 {
    match (message.reactions.is_empty(), reactions_overlap(message)) {
        (true, _) => 0.,
        (false, true) => REACTION_PILL - REACTION_OVERLAP,
        (false, false) => REACTION_PILL + REACTION_GAP,
    }
}

/// A sender's name in a group bubble, then their member label as a pill
/// tinted with the name's colour (Desktop's `GroupMemberLabel`).
fn author_line(name: String, color: Hsla, label: Option<signal_store::MemberLabel>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .min_w_0()
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(color).truncate().child(name))
        .when_some(label, |d, label| {
            d.child(
                div()
                    .debug_selector(|| "member-label".into())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px_1p5()
                    .rounded_full()
                    .bg(color.opacity(0.15))
                    .text_xs()
                    .text_color(color)
                    .when_some(label.emoji, |d, emoji| d.child(emoji))
                    .child(label.text),
            )
        })
}

pub(crate) fn typing_dot(t: f32, ix: usize) -> (f32, f32) {
    let phase = (t - ix as f32 * 0.1).rem_euclid(1.);
    // 0 → 20%: rise; 20% → 40%: fall; then rest.
    let rise = if phase < 0.2 { phase / 0.2 } else if phase < 0.4 { 1. - (phase - 0.2) / 0.2 } else { 0. };
    (0.4 + 0.6 * rise, 1. + 0.3 * rise)
}

/// Reaction pills shown before collapsing into "+N" (Signal Desktop's limit).
const MAX_REACTION_PILLS: usize = 3;

/// Reactions on a message grouped by emoji: most used first, ties by who
/// reacted first.
struct ReactionGroup {
    emoji: String,
    count: usize,
    mine: bool,
}

fn reaction_groups(reactions: &[Reaction]) -> Vec<ReactionGroup> {
    // emoji -> (count, includes mine, first reacted)
    let mut groups: BTreeMap<&str, (usize, bool, i64)> = BTreeMap::new();
    for Reaction { emoji, author, sent_at } in reactions {
        let entry = groups.entry(emoji.as_str()).or_insert((0, false, *sent_at));
        entry.0 += 1;
        entry.1 |= author.is_none();
        entry.2 = entry.2.min(*sent_at);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.1.2.cmp(&b.1.2)));
    groups
        .into_iter()
        .map(|(emoji, (count, mine, _))| ReactionGroup { emoji: emoji.to_owned(), count, mine })
        .collect()
}

/// Corner radii of a bubble. In a run of messages from one sender, the
/// corners on the sender's side where bubbles meet are tighter, so the run
/// reads as one block (Signal Desktop's grouping).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Corners {
    top_left: f32,
    top_right: f32,
    bottom_left: f32,
    bottom_right: f32,
}

impl Corners {
    const GROUPED: f32 = 6.;

    fn for_bubble(outgoing: bool, first: bool, last: bool) -> Self {
        let top = if first { BUBBLE_RADIUS } else { Self::GROUPED };
        let bottom = if last { BUBBLE_RADIUS } else { Self::GROUPED };
        if outgoing {
            Self { top_left: BUBBLE_RADIUS, top_right: top, bottom_left: BUBBLE_RADIUS, bottom_right: bottom }
        } else {
            Self { top_left: top, top_right: BUBBLE_RADIUS, bottom_left: bottom, bottom_right: BUBBLE_RADIUS }
        }
    }

    fn apply<E: Styled>(self, element: E) -> E {
        element
            .rounded_tl(px(self.top_left))
            .rounded_tr(px(self.top_right))
            .rounded_bl(px(self.bottom_left))
            .rounded_br(px(self.bottom_right))
    }
}

/// Invisible text about the width of the time and status, appended to the
/// message. If the last line has room it sits there and the time overlays
/// it; if not, it wraps to a new line and makes room for the time below.
/// It's set in the body's larger font, so the time's own characters cover
/// the gap before it; no-break spaces stand in for icons.
fn meta_spacer(message: &Message, cx: &App) -> String {
    // Measured, not guessed: the bubble ends where the spacer does, so any
    // slack would show as a gap before the time.
    let digit = text_width(cx, "0", BODY_SIZE);
    let wanted = meta_width(message, cx) + META_GAP_BEFORE - text_width(cx, " ", BODY_SIZE);
    let count = (wanted / digit).ceil().max(1.) as usize;
    // A breakable space, then one unbreakable word (spaces would hang past
    // the edge instead of wrapping).
    format!(" {}", "0".repeat(count))
}

/// Body text size and line height in bubbles.
const BODY_SIZE: f32 = 14.5;
const BODY_LINE: f32 = 20.;
/// Signal Desktop's metadata text size.
const META_SIZE: f32 = 12.;
/// Space between the text and the time on a shared line (Desktop: ~8pt).
const META_GAP_BEFORE: f32 = 8.;

/// Width of `text` in the UI font, from glyph advances.
fn text_width(cx: &App, text: &str, size: f32) -> f32 {
    let system = cx.text_system();
    let font = system.resolve_font(&gpui_kit::font(".SystemUIFont"));
    text.chars()
        .map(|c| system.advance(font, px(size), c).map_or(size * 0.55, |a| f32::from(a.width)))
        .sum()
}

/// Width of what [`render_meta`] draws.
fn meta_width(message: &Message, cx: &App) -> f32 {
    let outgoing = message.direction == Direction::Outgoing;
    let mut parts = vec![text_width(cx, &message_time(message.sent_at), META_SIZE)];
    if message.edited {
        parts.push(text_width(cx, "Edited ·", META_SIZE));
    }
    if message.expires_in > 0 {
        parts.push(12.);
    }
    if message.status == DeliveryStatus::Failed && outgoing {
        parts.push(text_width(cx, "Not sent", META_SIZE));
    }
    let mut width = parts.iter().sum::<f32>() + META_GAP * (parts.len() - 1) as f32;
    if outgoing && message.status != DeliveryStatus::Failed {
        let icon = if matches!(message.status, DeliveryStatus::Delivered | DeliveryStatus::Read) { 18. } else { 12. };
        width += META_GAP + STATUS_GAP + icon;
    }
    width
}

/// Gaps inside the metadata: between parts, and extra before the status icon
/// (Desktop leaves 6pt there).
const META_GAP: f32 = 4.;
const STATUS_GAP: f32 = 2.;

/// The time and status: on the bubble's last line of text (next to
/// [`meta_spacer`]), or on a line of its own. Either way it gets a box one
/// text line tall so its baseline lines up with the text's, as in Desktop.
fn place_meta(meta: impl IntoElement, inline: bool) -> AnyElement {
    let line = div().h(px(BODY_LINE)).flex().items_center().justify_end().pt(px(1.)).child(meta);
    if inline {
        // The bubble's bottom padding is 8pt; the last line sits right above.
        line.absolute().bottom(px(8.)).right(px(12.)).into_any_element()
    } else {
        line.into_any_element()
    }
}

/// Message text with its web links underlined and clickable, and room for
/// the time at the end when `spacer` is given (with the bubble's colour, to
/// draw it in: gpui blends highlight colours over the text colour, so a
/// transparent one would still show).
fn message_text(message_id: i64, body: &str, mentions: &[signal_store::MentionSpan], spacer: Option<(&str, Hsla)>) -> AnyElement {
    let text = div().text_size(px(BODY_SIZE)).line_height(px(BODY_LINE));
    let links = find_links(body);
    if links.is_empty() && mentions.is_empty() && spacer.is_none() {
        return text.child(body.to_owned()).into_any_element();
    }
    let underline = gpui_kit::HighlightStyle {
        underline: Some(gpui_kit::UnderlineStyle { thickness: px(1.), ..Default::default() }),
        ..Default::default()
    };
    // Desktop shows @mentions as a tinted pill; a soft background works in
    // both bubble colours.
    let mention = gpui_kit::HighlightStyle {
        font_weight: Some(FontWeight::SEMIBOLD),
        background_color: Some(gpui_kit::hsla(0., 0., 0.5, 0.22)),
        ..Default::default()
    };
    let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
    let mut content = body.to_owned();
    let mut highlights: Vec<_> = ranges.iter().map(|r| (r.clone(), underline)).collect();
    highlights.extend(
        mentions
            .iter()
            .filter(|m| body.is_char_boundary(m.start) && body.is_char_boundary(m.start + m.len) && m.start + m.len <= body.len())
            .map(|m| (m.start..m.start + m.len, mention)),
    );
    // gpui wants highlights in order and not overlapping.
    highlights.sort_by_key(|(r, _)| r.start);
    highlights.dedup_by(|b, a| b.0.start < a.0.end);
    if let Some((spacer, background)) = spacer {
        let invisible = gpui_kit::HighlightStyle { color: Some(background), ..Default::default() };
        highlights.push((content.len()..content.len() + spacer.len(), invisible));
        content.push_str(spacer);
    }
    let styled = gpui_kit::StyledText::new(content).with_highlights(highlights);
    if ranges.is_empty() {
        return text.child(styled).into_any_element();
    }
    text.child(
        gpui_kit::InteractiveText::new(("message-text", message_id as u64), styled)
            .on_click(ranges, move |ix, _, cx| cx.open_url(&urls[ix])),
    )
    .into_any_element()
}

/// Display size of a photo, following Signal Desktop's single-image limits:
/// 300pt wide (at least 200 for small images), height from the aspect ratio
/// within 50–450pt; the rest is cropped.
fn media_size(width: Option<u32>, height: Option<u32>) -> (f32, f32) {
    const MAX_WIDTH: f32 = 300.;
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let display = (w as f32).clamp(200., MAX_WIDTH);
            (display, (display * h as f32 / w as f32).clamp(50., 450.))
        }
        _ => (MAX_WIDTH, 225.),
    }
}

/// Side of the image beside a quote (the quoted photo or preview image).
const QUOTE_THUMBNAIL: f32 = 54.;

fn render_quote(quote: signal_store::Quote, fg: Hsla, palette: Palette) -> impl IntoElement {
    // A column with a left border, not a row with a bar beside the text: in a
    // flex row the text is measured unwrapped and would widen the bubble past
    // its maximum. A thumbnail is placed over reserved padding for the same
    // reason.
    let thumbnail = quote.thumbnail.clone();
    div()
        .relative()
        .flex()
        .flex_col()
        .px_2()
        .py_1()
        .when(thumbnail.is_some(), |d| d.pr(px(QUOTE_THUMBNAIL + 8.)).min_h(px(QUOTE_THUMBNAIL)))
        .when_some(thumbnail, |d, path| {
            d.child(
                img(std::path::PathBuf::from(path))
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(QUOTE_THUMBNAIL))
                    .rounded_r_md()
                    .object_fit(gpui_kit::ObjectFit::Cover),
            )
        })
        .rounded_md()
        .overflow_hidden()
        .border_l_4()
        .border_color(fg)
        .bg(Hsla { a: 0.15, ..palette.text })
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(if quote.author.is_none() { "You".to_string() } else { quote.author_name }),
        )
        .child(div().text_sm().line_clamp(3).text_ellipsis().child(collapse_whitespace(&quote.text)))
}

/// What the image viewer needs to show a downloaded image.
fn viewed_image(attachment: &AttachmentMeta) -> Option<ViewedImage> {
    Some(ViewedImage {
        path: std::path::PathBuf::from(attachment.local_path.as_ref()?),
        size: attachment.width.zip(attachment.height).filter(|(w, h)| *w > 0 && *h > 0),
        file_name: attachment.file_name.clone(),
    })
}

fn render_attachment(
    message_id: i64,
    index: usize,
    attachment: &AttachmentMeta,
    fg: Hsla,
    palette: Palette,
    cx: &mut Context<ConversationView>,
) -> AnyElement {
    let is_image = attachment.content_type.starts_with("image/");
    if let (true, Some(path)) = (is_image, attachment.local_path.as_ref()) {
        let (w, h) = match (attachment.width, attachment.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                let scale = (300. / w as f32).min(360. / h as f32).min(1.);
                (w as f32 * scale, h as f32 * scale)
            }
            _ => (300., 220.),
        };
        let viewed = viewed_image(attachment);
        return div()
            .id(SharedString::from(format!("attachment-{message_id}-{index}")))
            .cursor_pointer()
            .when_some(viewed, |d, viewed| {
                d.on_click(cx.listener(move |this, _, _, cx| {
                    let image = viewed.clone();
                    this.state.update(cx, |state, cx| state.view_image(image, cx));
                }))
            })
            .child(img(std::path::PathBuf::from(path)).w(px(w)).h(px(h)).rounded_lg())
            .into_any_element();
    }
    let name = attachment
        .file_name
        .clone()
        .unwrap_or_else(|| attachment.content_type.clone());
    div()
        .id(SharedString::from(format!("attachment-{message_id}-{index}")))
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .rounded_lg()
        .bg(Hsla { a: 0.12, ..palette.text })
        .cursor_pointer()
        .child(Icon::new(if is_image { IconName::Image } else { IconName::File }).size_5().text_color(fg))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(div().text_sm().truncate().child(name))
                .child(div().text_xs().child(if attachment.local_path.is_some() {
                    crate::views::common::format_size(attachment.size)
                } else {
                    format!("{} · Click to download", crate::views::common::format_size(attachment.size))
                })),
        )
        .on_click(cx.listener({
            let local = attachment.local_path.clone().map(std::path::PathBuf::from);
            move |this, _, _, cx| match &local {
                // Downloaded (or ours): open it in its app.
                Some(path) => cx.open_with_system(path),
                None => this.state.read(cx).send(Command::FetchAttachment { message_id, index }),
            }
        }))
        .into_any_element()
}

fn format_timer(seconds: u32) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s => format!("{}w", s / 604_800),
    }
}

impl Focusable for ConversationView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }
}

impl Render for ConversationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let entity = cx.entity();
        let typing = self.render_typing(palette, cx);
        let scrolled_up =
            self.has_newer || (!self.list.is_following_tail() && !self.list.is_scrolled_to_end().unwrap_or(true));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .child(self.render_header(palette, cx))
            .children(self.render_pins(palette, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .image_cache(self.image_cache.clone())
                    .child(
                        list(self.list.clone(), move |ix, _window, cx| {
                            entity.update(cx, |this, cx| {
                                let palette = Palette::get(cx);
                                this.render_item(ix, palette, cx)
                            })
                        })
                        .size_full(),
                    )
                    .when(scrolled_up, |d| {
                        d.child(
                            div()
                                .id("jump-to-latest")
                                .debug_selector(|| "jump-to-latest".into())
                                .absolute()
                                .bottom_4()
                                .right_4()
                                .size(px(40.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(palette.popover)
                                .border_1()
                                .border_color(palette.panel_border)
                                .shadow_md()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this.has_newer {
                                        // Jumped back: reload the newest.
                                        this.load_initial(cx);
                                        this.caught_up(cx);
                                        return;
                                    }
                                    this.list.set_follow_mode(FollowMode::Tail);
                                    this.list.scroll_to_end();
                                    this.caught_up(cx);
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ArrowDown).size_5().text_color(palette.text))
                                // How many arrived while you were scrolled up.
                                .when(self.unseen > 0, |d| {
                                    d.child(
                                        div()
                                            .absolute()
                                            .top(px(-6.))
                                            .right(px(-6.))
                                            .min_w(px(20.))
                                            .h(px(20.))
                                            .px_1()
                                            .rounded_full()
                                            .bg(palette.badge)
                                            .text_color(palette.on_accent)
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(self.unseen.to_string()),
                                    )
                                }),
                        )
                    })
                    .when(self.reaction_picker_for.is_some(), |d| {
                        d.child(deferred(
                            div()
                                .absolute()
                                .bottom_2()
                                .right_4()
                                .child(self.picker.clone()),
                        ))
                    }),
            )
            .when_some(typing, |d, t| d.child(t))
            .map(|d| {
                if let Some(selected) = &self.selecting {
                    return d.child(self.render_select_bar(selected, palette, cx));
                }
                match self.state.read(cx).conversation(&self.id).filter(|c| c.blocked || c.left) {
                    Some(conversation) if conversation.blocked => d.child(self.render_blocked(conversation, palette)),
                    // Desktop: "You are no longer a member of the group."
                    Some(_) => d.child(
                        div()
                            .flex_none()
                            .px_6()
                            .py_4()
                            .border_t_1()
                            .border_color(palette.panel_border)
                            .text_sm()
                            .text_center()
                            .text_color(palette.text_secondary)
                            .child("You are no longer a member of the group."),
                    ),
                    None => d.child(self.composer.clone()),
                }
            })
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Desktop's "⋯" message menu (also on right-click): Forward, Select, Copy
/// text, Info, Delete, and Retry Send for a failed message.
fn message_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    message: &Message,
    view: gpui_kit::WeakEntity<ConversationView>,
    cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu {
    use gpui_kit::component::menu::PopupMenuItem;
    let Some(this) = view.upgrade() else { return menu };
    let id = message.id;
    let (state, note_to_self) = this.read_with(cx, |this, cx| (this.state.clone(), this.note_to_self(cx)));
    let for_everyone = signal_backend::deletes::can_send(message, note_to_self, now_millis());
    let failed = message.direction == Direction::Outgoing && message.status == DeliveryStatus::Failed;
    let with_view = |f: fn(&mut ConversationView, i64, &mut Window, &mut Context<ConversationView>)| {
        let view = view.clone();
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = view.update(cx, |this, cx| f(this, id, window, cx));
        }
    };
    let mut menu = menu;
    if !message.deleted {
        let state = state.clone();
        menu = menu.item(
            PopupMenuItem::new("Forward")
                .icon(IconName::Forward)
                .on_click(move |_, window, cx| crate::views::forward::open(state.clone(), vec![id], window, cx)),
        );
    }
    if signal_backend::edits::can_edit(message, note_to_self, now_millis()) {
        let editing = message.clone();
        let view = view.clone();
        menu = menu.item(PopupMenuItem::new("Edit").icon(IconName::Pencil).on_click(move |_, window, cx| {
            let editing = editing.clone();
            let _ = view.update(cx, |this, cx| this.composer.update(cx, |composer, cx| composer.start_edit(editing, window, cx)));
        }));
    }
    let (pinned, can_pin, pin_count) = this.read_with(cx, |this, cx| {
        let can = this.state.read(cx).conversation(&this.id).is_some_and(|c| c.can_edit_info);
        (this.pins.iter().any(|p| p.id == id), can, this.pins.len())
    });
    if can_pin && !message.deleted {
        let state = state.clone();
        menu = if pinned {
            menu.item(
                PopupMenuItem::new("Unpin")
                    .icon(IconName::PinOff)
                    .on_click(move |_, _, cx| state.read(cx).send(Command::UnpinMessage { message_id: id })),
            )
        } else {
            let disappearing = message.expires_in > 0;
            menu.item(PopupMenuItem::new("Pin").icon(IconName::Pin).on_click(move |_, window, cx| {
                open_pin_dialog(state.clone(), id, pin_count, disappearing, window, cx)
            }))
        };
    }
    menu = menu.item(
        PopupMenuItem::new("Select")
            .icon(IconName::SquareCheck)
            .on_click(with_view(|this, id, _, cx| this.start_selecting(Some(id), cx))),
    );
    if !message.body.is_empty() && !message.deleted {
        let body = message.body.clone();
        menu = menu.item(
            PopupMenuItem::new("Copy text")
                .icon(IconName::Copy)
                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(body.clone()))),
        );
    }
    let details = message.clone();
    menu = menu.item(
        PopupMenuItem::new("Info")
            .icon(IconName::Info)
            .on_click(move |_, window, cx| crate::views::message_info::open(details.clone(), window, cx)),
    );
    let delete_state = state.clone();
    menu = menu.item(PopupMenuItem::new("Delete").icon(IconName::Trash).on_click(move |_, window, cx| {
        confirm_delete(delete_state.clone(), vec![id], note_to_self, for_everyone, window, cx)
    }));
    if failed {
        menu = menu.item(
            PopupMenuItem::new("Retry Send")
                .icon(IconName::RotateCcw)
                .on_click(move |_, _, cx| state.read(cx).send(Command::RetrySend { message_id: id })),
        );
    }
    menu
}

/// Desktop's "Pin message for..." dialog: how long, then (with three
/// pinned already) "Replace oldest pin?".
fn open_pin_dialog(state: Entity<AppState>, message_id: i64, pinned: usize, disappearing: bool, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    const CHOICES: [(&str, Option<u32>); 4] =
        [("24 hours", Some(86_400)), ("7 days", Some(7 * 86_400)), ("30 days", Some(30 * 86_400)), ("Forever", None)];
    let choice = cx.new(|_| 0usize);
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let picked = *choice.read(cx);
        let (state, confirm) = (state.clone(), choice.clone());
        dialog
            .title("Pin message for...")
            .w(px(380.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(disappearing, |d| {
                        d.child(div().pb_2().text_sm().text_color(palette.text_secondary).child(
                            "Disappearing messages will be unpinned when their timer expires and the message is removed from the chat.",
                        ))
                    })
                    .children(CHOICES.iter().enumerate().map(|(ix, (label, _))| {
                        let choice = choice.clone();
                        div()
                            .id(("pin-choice", ix))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_2()
                            .py_1p5()
                            .rounded_lg()
                            .cursor_pointer()
                            .hover(|d| d.bg(palette.row_hover))
                            .on_click(move |_, window, cx| {
                                choice.update(cx, |c, _| *c = ix);
                                window.refresh();
                            })
                            .child(
                                div()
                                    .size(px(16.))
                                    .rounded_full()
                                    .border_2()
                                    .border_color(if ix == picked { palette.accent } else { palette.text_secondary })
                                    .when(ix == picked, |d| d.bg(palette.accent)),
                            )
                            .child(*label)
                    })),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("pin-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("pin-confirm").label("Pin").primary().on_click(move |_, window, cx| {
                        let duration = CHOICES[*confirm.read(cx)].1;
                        window.close_all_dialogs(cx);
                        if pinned < signal_backend::MAX_PINNED_MESSAGES {
                            state.read(cx).send(Command::PinMessage { message_id, duration });
                            return;
                        }
                        let state = state.clone();
                        window.open_alert_dialog(cx, move |alert, _, _| {
                            let state = state.clone();
                            alert
                                .title("Replace oldest pin?")
                                .description("Pinning this message will replace the oldest one.")
                                .confirm()
                                .ok_text("Continue")
                                .on_ok(move |_, window, cx| {
                                    state.read(cx).send(Command::PinMessage { message_id, duration });
                                    window.close_all_dialogs(cx);
                                    true
                                })
                        });
                    })),
            )
    });
}

/// Saves downloaded attachments: one through a save dialog, several into a
/// chosen folder.
fn download_attachments(attachments: Vec<AttachmentMeta>, cx: &mut App) {
    let downloads = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Downloads")).unwrap_or_default();
    let name = |a: &AttachmentMeta| {
        let path = std::path::PathBuf::from(a.local_path.clone().unwrap_or_default());
        a.file_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| {
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into())
        })
    };
    if let [one] = attachments.as_slice() {
        let source = one.local_path.clone().unwrap_or_default();
        let chosen = cx.prompt_for_new_path(&downloads, Some(&name(one)));
        cx.spawn(async move |_| {
            if let Ok(Ok(Some(path))) = chosen.await
                && let Err(error) = std::fs::copy(&source, &path)
            {
                tracing::warn!(%error, "failed to save attachment");
            }
        })
        .detach();
        return;
    }
    let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Save".into()),
    });
    let files: Vec<(String, String)> =
        attachments.iter().map(|a| (a.local_path.clone().unwrap_or_default(), name(a))).collect();
    cx.spawn(async move |_| {
        let Ok(Ok(Some(dirs))) = chosen.await else { return };
        let Some(dir) = dirs.into_iter().next() else { return };
        for (source, name) in files {
            // Don't overwrite: "photo.jpg", "photo (1).jpg", …
            if let Err(error) = crate::views::image_viewer::save_copy(std::path::Path::new(&source), &dir, &name) {
                tracing::warn!(%error, "failed to save attachment");
            }
        }
    })
    .detach();
}

/// Desktop's delete dialog: for you (on all your devices), or for everyone
/// while that's still allowed. Note to Self only has the first.
fn confirm_delete(
    state: Entity<AppState>,
    message_ids: Vec<i64>,
    note_to_self: bool,
    for_everyone: bool,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    let count = message_ids.len();
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let (one, these) = if count == 1 { ("message", "this message") } else { ("messages", "these messages") };
        let (mine, everyone) = (state.clone(), state.clone());
        let (my_ids, all_ids) = (message_ids.clone(), message_ids.clone());
        dialog
            .title(if count == 1 { "Delete selected message?".to_owned() } else { format!("Delete selected {count} {one}?") })
            .w(px(420.))
            .child(div().text_sm().text_color(palette.text_secondary).child(if note_to_self {
                format!("{} will be deleted from all your devices.", capitalize(these))
            } else {
                format!("Who would you like to delete {these} for?")
            }))
            .footer(
                DialogFooter::new()
                    .child(Button::new("delete-cancel").label("Cancel").outline().on_click(|_, window, cx| {
                        window.close_all_dialogs(cx);
                    }))
                    .child(
                        Button::new("delete-for-me")
                            .label(if note_to_self { "Delete" } else { "Delete for me" })
                            .danger()
                            .on_click(move |_, window, cx| {
                                mine.read(cx).send(Command::DeleteForMe { message_ids: my_ids.clone() });
                                window.close_all_dialogs(cx);
                            }),
                    )
                    .when(for_everyone && !note_to_self, |footer| {
                        footer.child(
                            Button::new("delete-for-everyone")
                                .label("Delete for everyone")
                                .danger()
                                .on_click(move |_, window, cx| {
                                    for message_id in &all_ids {
                                        everyone.read(cx).send(Command::DeleteForEveryone { message_id: *message_id });
                                    }
                                    window.close_all_dialogs(cx);
                                }),
                        )
                    }),
            )
    });
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Bounds, Pixels, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, px, size};
    use signal_backend::{Backend, Config};

    use super::*;
    use crate::views::emoji_picker::EmojiPicker;

    const NOTE_TO_SELF: &str = "d:00000000-0000-4000-8000-000000000001";
    const GROUP: &str = "g:0000000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn media_size_follows_desktop_limits() {
        assert_eq!(media_size(Some(600), Some(800)), (300., 400.));
        assert_eq!(media_size(Some(4000), Some(1000)), (300., 75.));
        // Very tall images are cropped; very wide ones keep a minimum height.
        assert_eq!(media_size(Some(600), Some(6000)), (300., 450.));
        assert_eq!(media_size(Some(6000), Some(100)), (300., 50.));
        // Small images grow to 200 wide.
        assert_eq!(media_size(Some(100), Some(100)), (200., 200.));
        assert_eq!(media_size(None, None), (300., 225.));
    }

    fn demo_backend(cx: &mut TestAppContext) -> (Backend, tempfile::TempDir) {
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
        (backend, dir)
    }

    /// The conversation's messages once the demo has seeded at least `count`
    /// matching `filter`.
    fn wait_for_messages(backend: &Backend, id: &str, count: usize, filter: impl Fn(&Message) -> bool) -> Vec<Message> {
        for _ in 0..300 {
            let db = backend.db().clone();
            let conversation = ConversationId(id.into());
            let messages: Vec<Message> =
                futures::executor::block_on(backend.spawn(async move { db.messages(&conversation, None, 50).await }))
                    .unwrap_or_default()
                    .into_iter()
                    .filter(&filter)
                    .collect();
            if messages.len() >= count {
                return messages;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("demo messages weren't seeded");
    }

    fn open_view(cx: &mut TestAppContext, backend: &Backend, id: &str) -> (VisualTestContext, Entity<ConversationView>) {
        open_view_sized(cx, backend, id, 2400.)
    }

    fn open_view_sized(
        cx: &mut TestAppContext,
        backend: &Backend,
        id: &str,
        height: f32,
    ) -> (VisualTestContext, Entity<ConversationView>) {
        let id = ConversationId(id.into());
        let (window, view) = cx.update(|cx| {
            let state = crate::state::new_state(backend.clone(), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(900.), px(height)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
                cx.new(|cx| ConversationView::new(state, picker, id, window, cx))
            })
            .unwrap()
        });
        (VisualTestContext::from_window(window, cx), view)
    }

    fn open(cx: &mut TestAppContext, backend: &Backend, id: &str) -> VisualTestContext {
        open_view(cx, backend, id).0
    }

    fn bounds(cx: &mut VisualTestContext, selector: String) -> Bounds<Pixels> {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        for _ in 0..300 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            if let Some(b) = cx.debug_bounds(selector) {
                return b;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{selector} never rendered");
    }

    #[gpui_kit::test]
    fn photo_bubbles_are_as_wide_as_the_photo(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let photos = wait_for_messages(&backend, NOTE_TO_SELF, 3, |m| !m.attachments.is_empty());
        let mut cx = open(cx, &backend, NOTE_TO_SELF);

        // Photo only, short caption, long caption: all as wide as the photo
        // (the window would allow ~600pt), and a bare photo has no padding.
        let bare = bounds(&mut cx, format!("bubble-{}", photos[0].id));
        assert_eq!((bare.size.width, bare.size.height), (px(300.), px(400.)));
        assert_eq!(bounds(&mut cx, format!("bubble-{}", photos[1].id)).size.width, px(300.));
        let long = bounds(&mut cx, format!("bubble-{}", photos[2].id));
        assert_eq!(long.size.width, px(300.));
        assert!(long.size.height > px(500.), "caption should wrap to the photo's width: {long:?}");
        backend.shutdown();
    }

    const MAYA: &str = "d:00000000-0000-4000-8000-00000000a11c";

    #[test]
    fn grouped_bubbles_tighten_the_senders_side() {
        let alone = Corners::for_bubble(false, true, true);
        assert_eq!(alone, Corners { top_left: 18., top_right: 18., bottom_left: 18., bottom_right: 18. });
        // Incoming, middle of a run: left (sender's) side tight.
        let middle = Corners::for_bubble(false, false, false);
        assert_eq!(middle, Corners { top_left: 6., top_right: 18., bottom_left: 6., bottom_right: 18. });
        // Outgoing, first of a run: only the bottom right meets the next one.
        let first = Corners::for_bubble(true, true, false);
        assert_eq!(first, Corners { top_left: 18., top_right: 18., bottom_left: 18., bottom_right: 6. });
    }

    #[gpui_kit::test]
    fn short_messages_keep_the_time_on_their_line(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let messages = wait_for_messages(&backend, MAYA, 1, |m| m.body == "Did you get home okay?");
        let mut cx = open(cx, &backend, MAYA);
        // Padding (8 + 8) plus one 20pt line: the time didn't add a line.
        let bubble = bounds(&mut cx, format!("bubble-{}", messages[0].id));
        assert_eq!(bubble.size.height, px(36.), "{bubble:?}");
        backend.shutdown();
    }

    #[test]
    fn full_size_previews_follow_desktop() {
        let preview = |width, height| signal_store::LinkPreview {
            url: "https://signal.org/".into(),
            title: None,
            description: None,
            date: None,
            image: Some(AttachmentMeta {
                content_type: "image/png".into(),
                file_name: None,
                size: 1,
                width: Some(width),
                height: Some(height),
                local_path: Some("/tmp/x.png".into()),
                sticker: false,
                gif: false,
                remote: None,
            }),
        };
        assert_eq!(full_size_preview_image(&preview(1200, 630)).map(|f| f.1), Some((300., 157.5)));
        // Too small, or square: a thumbnail instead.
        assert!(full_size_preview_image(&preview(160, 1200)).is_none());
        assert!(full_size_preview_image(&preview(500, 510)).is_none());
    }

    #[gpui_kit::test]
    fn link_previews_render_full_size_or_beside_the_text(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let trail = wait_for_messages(&backend, GROUP, 1, |m| m.link_preview.is_some()).remove(0);
        let mut group = open(cx, &backend, GROUP);
        // 600×315 image: the bubble takes its 300pt width.
        assert_eq!(bounds(&mut group, format!("bubble-{}", trail.id)).size.width, px(300.));

        let messages = wait_for_messages(&backend, MAYA, 2, |m| m.link_preview.is_some() || m.body == "Reading it now");
        let mut alice = open(cx, &backend, MAYA);
        let notes = messages.iter().find(|m| m.link_preview.is_some()).unwrap();
        let reply = messages.iter().find(|m| m.body == "Reading it now").unwrap();
        // Square image: a 72pt thumbnail beside the text, inside its card.
        let bubble = bounds(&mut alice, format!("bubble-{}", notes.id));
        assert!(bubble.size.height >= px(72. + 16. + 36.), "{bubble:?}");
        // A reply to it shows the thumbnail without widening past the window.
        let reply = bounds(&mut alice, format!("bubble-{}", reply.id));
        assert!(reply.size.width < px(700.) && reply.size.height >= px(54.), "{reply:?}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn messages_are_read_only_once_scrolled_into_view(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 3, |_| true);
        // Short, so the chat always overflows (images still loading change
        // its height).
        let (mut cx, view) = open_view_sized(cx, &backend, MAYA, 260.);
        let unread = |backend: &Backend| {
            let db = backend.db().clone();
            let id = ConversationId(MAYA.into());
            futures::executor::block_on(backend.spawn(async move { db.conversation(&id).await }))
                .unwrap()
                .unwrap()
                .unread_count
        };
        let settle = |cx: &mut VisualTestContext| {
            for _ in 0..30 {
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let arrive = |cx: &mut VisualTestContext, backend: &Backend, sent_at: i64| {
            let db = backend.db().clone();
            let message = signal_store::NewMessage {
                conversation_id: ConversationId(MAYA.into()),
                sent_at,
                received_at: sent_at,
                direction: Direction::Incoming,
                author: Some(MAYA.trim_start_matches("d:").into()),
                author_name: Some("Maya Chen".into()),
                body: format!("new {sent_at}"),
                status: DeliveryStatus::Delivered,
                quote: None,
                attachments: vec![],
                link_preview: None,
                expires_in: 0,
            };
            let id = futures::executor::block_on(backend.spawn(async move { db.insert_message(&message).await })).unwrap();
            view.update(cx, |v, cx| v.on_message_changed(id, cx));
        };
        cx.update(|_, cx| view.update(cx, |v, _| v.window_active = true));
        // Loading lands at the bottom; scrolling before it would be undone.
        for _ in 0..300 {
            settle(&mut cx);
            if view.read_with(&cx, |v, _| v.messages.len() >= 3 && !v.loading_older) {
                break;
            }
        }

        // At the bottom: an arriving message is read at once.
        arrive(&mut cx, &backend, chrono::Utc::now().timestamp_millis());
        settle(&mut cx);
        assert_eq!(unread(&backend), 0);

        // Scrolled up: it waits, counted on the jump-to-latest button…
        view.update(&mut cx, |v, _| {
            v.list.set_follow_mode(FollowMode::Normal);
            v.list.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
        });
        // Under load the scroll can take more than one settle to land.
        for _ in 0..50 {
            settle(&mut cx);
            if !view.read_with(&cx, |v, _| v.at_end()) {
                break;
            }
        }
        assert!(!view.read_with(&cx, |v, _| v.at_end()), "scrolled up");
        arrive(&mut cx, &backend, chrono::Utc::now().timestamp_millis() + 1);
        settle(&mut cx);
        assert_eq!(unread(&backend), 1);
        assert_eq!(view.read_with(&cx, |v, _| v.unseen), 1);

        // …until you go down to it.
        let jump = bounds(&mut cx, "jump-to-latest".into());
        cx.simulate_click(jump.center(), gpui_kit::Modifiers::none());
        settle(&mut cx);
        assert_eq!(unread(&backend), 0);
        assert_eq!(view.read_with(&cx, |v, _| v.unseen), 0);
        backend.shutdown();
    }

    #[test]
    fn typing_dots_pulse_in_turn() {
        // Resting, then the first dot peaks at 20% while the third waits.
        assert_eq!(typing_dot(0., 0), (0.4, 1.));
        let (peak, scale) = typing_dot(0.2, 0);
        assert!((peak - 1.).abs() < 1e-5 && (scale - 1.3).abs() < 1e-5);
        assert_eq!(typing_dot(0.2, 2), (0.4, 1.));
        let (third, _) = typing_dot(0.4, 2);
        assert!((third - 1.).abs() < 1e-5);
    }

    #[gpui_kit::test]
    fn group_typing_shows_the_bubble(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, GROUP, 1, |_| true);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        for n in 0..4 {
            state.update(&mut cx, |state, cx| {
                state.handle(
                    signal_backend::Event::Typing {
                        conversation: ConversationId(GROUP.into()),
                        author: format!("00000000-0000-4000-8000-00000000000{n}"),
                        started: true,
                    },
                    cx,
                )
            });
        }
        let bubble = bounds(&mut cx, "typing-bubble".into());
        // Avatars, then a 36pt bubble.
        assert!(bubble.size.height >= px(36.), "{bubble:?}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn avatars_sit_level_with_bubbles_that_have_reactions(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let view_photo = wait_for_messages(&backend, GROUP, 1, |m| m.body == "View from the top last time. Bring layers, it's windy up there" && !m.reactions.is_empty())
            .remove(0);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        state.update(&mut cx, |state, cx| state.handle(signal_backend::Event::ConversationsChanged, cx));
        let avatar = bounds(&mut cx, format!("avatar-{}", view_photo.id));
        let bubble = bounds(&mut cx, format!("bubble-{}", view_photo.id));
        assert_eq!(avatar.bottom(), bubble.bottom(), "avatar {avatar:?} vs bubble {bubble:?}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn jumping_to_a_search_result_and_back(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 3, |_| true);
        // A long chat: 100 more messages, a minute apart.
        let db = backend.db().clone();
        let ids: Vec<i64> = futures::executor::block_on(backend.spawn(async move {
            let mut ids = Vec::new();
            for n in 0..100i64 {
                let message = signal_store::NewMessage {
                    conversation_id: ConversationId(MAYA.into()),
                    sent_at: 1_000_000 + n * 60_000,
                    received_at: 1_000_000 + n * 60_000,
                    direction: Direction::Incoming,
                    author: Some(MAYA.trim_start_matches("d:").into()),
                    author_name: Some("Maya Chen".into()),
                    body: format!("old {n}"),
                    status: DeliveryStatus::Read,
                    quote: None,
                    attachments: vec![],
                    link_preview: None,
                    expires_in: 0,
                };
                ids.push(db.insert_message(&message).await?);
            }
            Ok::<_, signal_store::Error>(ids)
        }))
        .unwrap();
        let (mut cx, view) = open_view_sized(cx, &backend, MAYA, 600.);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        let target = ids[5];
        state.update(&mut cx, |state, cx| state.jump_to(ConversationId(MAYA.into()), target, cx));
        let row = bounds(&mut cx, format!("row-{target}"));
        assert!(row.top() >= px(0.) && row.top() < px(600.), "on screen: {row:?}");
        view.read_with(&cx, |v, _| {
            assert_eq!(v.highlight, Some(target));
            assert!(v.has_newer, "newer messages are still to load");
        });

        let jump = bounds(&mut cx, "jump-to-latest".into());
        cx.simulate_click(jump.center(), gpui_kit::Modifiers::none());
        for _ in 0..300 {
            cx.run_until_parked();
            if view.read_with(&cx, |v, _| !v.has_newer && !v.loading_older) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        view.read_with(&cx, |v, _| {
            assert!(!v.has_newer);
            assert!(v.messages.last().is_some_and(|m| m.sent_at > 1_000_000 + 99 * 60_000), "back at the newest");
        });
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn select_mode_picks_messages_by_click(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let messages = wait_for_messages(&backend, MAYA, 2, |_| true);
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let (a, b) = (messages[0].id, messages[1].id);
        view.update(&mut cx, |v, cx| v.start_selecting(Some(a), cx));
        let row = bounds(&mut cx, format!("row-{b}"));
        cx.simulate_click(row.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        let selected = view.read_with(&cx, |v, _| v.selecting.clone().unwrap());
        assert_eq!(selected.into_iter().collect::<Vec<_>>(), {
            let mut both = vec![a, b];
            both.sort();
            both
        });
        // Clicking again deselects.
        cx.simulate_click(row.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(!view.read_with(&cx, |v, _| v.selecting.as_ref().unwrap().contains(&b)));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn forwarding_copies_a_message_to_other_chats(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let original = wait_for_messages(&backend, MAYA, 1, |m| !m.body.is_empty()).remove(0);
        backend.send(Command::Forward {
            message_ids: vec![original.id],
            conversations: vec![ConversationId(GROUP.into()), ConversationId(NOTE_TO_SELF.into())],
        });
        for chat in [GROUP, NOTE_TO_SELF] {
            let copy = wait_for_messages(&backend, chat, 1, |m| m.body == original.body && m.direction == Direction::Outgoing);
            assert_eq!(copy.len(), 1, "{chat}");
        }
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn up_arrow_edits_your_last_message(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, NOTE_TO_SELF, 1, |m| !m.body.is_empty());
        let (mut cx, view) = open_view(cx, &backend, NOTE_TO_SELF);
        let mut last = None;
        for _ in 0..300 {
            cx.run_until_parked();
            last = view.read_with(&cx, |v, _| v.messages.last().cloned());
            if last.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let last = last.expect("Note to Self has messages");
        assert!(!last.body.is_empty(), "the newest is text: {last:?}");
        // Our account makes this chat Note to Self (editable any time).
        let state = view.read_with(&cx, |v, _| v.state.clone());
        state.update(&mut cx, |state, _| {
            state.account = Some(crate::state::Account {
                phone: None,
                aci: NOTE_TO_SELF.trim_start_matches("d:").into(),
            })
        });
        cx.update(|window, cx| {
            let composer = view.read(cx).composer.clone();
            composer.update(cx, |c, cx| c.focus(window, cx));
        });
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        let editing = view.read_with(&cx, |v, cx| v.composer.read(cx).editing.as_ref().map(|m| m.id));
        assert_eq!(editing, Some(last.id), "↑ opened the last message for editing");
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("Edited text");
        let typed = view.read_with(&cx, |v, cx| v.composer.read(cx).input.read(cx).value().to_string());
        assert_eq!(typed, "Edited text");
        cx.simulate_keystrokes("enter");
        let edited = wait_for_messages(&backend, NOTE_TO_SELF, 1, |m| m.id == last.id && m.body == "Edited text").remove(0);
        assert!(edited.edited);
        assert_eq!(edited.edit_count, 1);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn pinning_shows_the_bar_and_a_notice(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let target = wait_for_messages(&backend, MAYA, 1, |m| !m.body.is_empty()).remove(0);
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        // (Tests deliver backend events by hand.)
        let pins_changed = |cx: &mut VisualTestContext| {
            state.update(cx, |state, cx| state.handle(signal_backend::Event::PinsChanged(ConversationId(MAYA.into())), cx));
        };
        backend.send(Command::PinMessage { message_id: target.id, duration: None });
        let notice =
            wait_for_messages(&backend, MAYA, 1, |m| matches!(m.notice, Some(signal_store::Notice::Pinned { .. }))).remove(0);
        pins_changed(&mut cx);
        bounds(&mut cx, "pinned-bar".into());
        assert!(matches!(notice.notice, Some(signal_store::Notice::Pinned { by: None, .. })));
        assert_eq!(view.read_with(&cx, |v, _| v.pins.iter().map(|p| p.id).collect::<Vec<_>>()), vec![target.id]);

        backend.send(Command::UnpinMessage { message_id: target.id });
        for _ in 0..300 {
            pins_changed(&mut cx);
            cx.run_until_parked();
            if view.read_with(&cx, |v, _| v.pins.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(view.read_with(&cx, |v, _| v.pins.is_empty()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn mentioning_a_group_member(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, GROUP, 1, |_| true);
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        // The composer reads the group's members from the chat list.
        state.update(cx, |state, cx| state.handle(signal_backend::Event::ConversationsChanged, cx));
        for _ in 0..300 {
            cx.run_until_parked();
            if state.read_with(cx, |s, _| s.conversation(&ConversationId(GROUP.into())).is_some_and(|c| !c.members.is_empty())) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
                cx.new(|cx| ConversationView::new(state.clone(), picker, ConversationId(GROUP.into()), window, cx))
            })
            .unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| {
            let composer = view.read(cx).composer.clone();
            composer.update(cx, |c, cx| c.focus(window, cx));
        });
        // Member names load in the background.
        for _ in 0..100 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.simulate_input("hi @May");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.simulate_input("see you there");
        cx.simulate_keystrokes("enter");
        let sent =
            wait_for_messages(&backend, GROUP, 1, |m| m.body.ends_with("see you there") && !m.mentions.is_empty()).remove(0);
        assert_eq!(sent.body, "hi @Maya Chen see you there");
        assert_eq!(sent.mentions.len(), 1);
        assert_eq!(&sent.body[sent.mentions[0].start..sent.mentions[0].start + sent.mentions[0].len], "@Maya Chen");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn member_labels_show_beside_names(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, GROUP, 1, |m| m.direction == Direction::Incoming);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        state.update(&mut cx, |state, cx| state.handle(signal_backend::Event::ConversationsChanged, cx));
        let pill = bounds(&mut cx, "member-label".into());
        assert!(pill.size.width > px(20.), "{pill:?}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn creating_a_group_opens_it(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, GROUP, 1, |_| true);
        backend.send(Command::CreateGroup { title: "Book club".into(), members: vec![MAYA.trim_start_matches("d:").into()] });
        let db = backend.db().clone();
        let mut found = None;
        for _ in 0..300 {
            let db = db.clone();
            let chats = futures::executor::block_on(backend.spawn(async move { db.conversations().await })).unwrap();
            found = chats.into_iter().find(|c| c.title == "Book club");
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let group = found.expect("the group was created");
        assert_eq!(group.kind, ConversationKind::Group);
        assert_eq!(group.members, vec![MAYA.trim_start_matches("d:").to_owned()]);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn typing_avatar_lines_up_with_message_avatars(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let incoming = wait_for_messages(&backend, GROUP, 1, |m| m.direction == Direction::Incoming);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        // Load the chat list, so the view knows this is a group.
        state.update(&mut cx, |state, cx| state.handle(signal_backend::Event::ConversationsChanged, cx));
        for _ in 0..300 {
            cx.run_until_parked();
            if state.read_with(&cx, |s, _| s.conversation(&ConversationId(GROUP.into())).is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        state.update(&mut cx, |state, cx| {
            state.handle(
                signal_backend::Event::Typing {
                    conversation: ConversationId(GROUP.into()),
                    author: "00000000-0000-4000-8000-000000000001".into(),
                    started: true,
                },
                cx,
            )
        });
        let typing = bounds(&mut cx, "typing-avatar-0".into());
        // Some incoming message ends its run and shows an avatar.
        let avatar = incoming
            .iter()
            .rev()
            .find_map(|m| {
                let selector: &'static str = Box::leak(format!("avatar-{}", m.id).into_boxed_str());
                cx.debug_bounds(selector)
            })
            .expect("a message avatar is on screen");
        assert_eq!(typing.left(), avatar.left(), "typing {typing:?} vs message {avatar:?}");
        let bubble = bounds(&mut cx, "typing-dots".into());
        assert_eq!(typing.bottom(), bubble.bottom(), "typing avatar level with its bubble");
        assert_eq!(typing.size, avatar.size);
        let single_bubble = bounds(&mut cx, "typing-dots".into());

        // A second typist: newest leftmost, the first 24pt to its right
        // (28pt overlapped by 4), and the bubble 24pt further over.
        state.update(&mut cx, |state, cx| {
            state.handle(
                signal_backend::Event::Typing {
                    conversation: ConversationId(GROUP.into()),
                    author: "00000000-0000-4000-8000-000000000002".into(),
                    started: true,
                },
                cx,
            )
        });
        let newest = bounds(&mut cx, "typing-avatar-0".into());
        let older = bounds(&mut cx, "typing-avatar-1".into());
        let bubble = bounds(&mut cx, "typing-dots".into());
        assert_eq!(newest.left(), avatar.left());
        assert_eq!(older.left() - newest.left(), px(24.));
        assert_eq!(bubble.left() - single_bubble.left(), px(24.));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn clicking_a_reaction_opens_the_viewer(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        // The outgoing plan message has four kinds of reaction.
        let plan = wait_for_messages(&backend, GROUP, 1, |m| m.reactions.len() >= 5).remove(0);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        let pills = bounds(&mut cx, format!("reactions-{}", plan.id));

        let more = bounds(&mut cx, format!("reaction-{}-more", plan.id));
        cx.simulate_click(more.center(), gpui_kit::Modifiers::none());
        let viewer = bounds(&mut cx, format!("reaction-viewer-{}", plan.id));
        // Above the pills when there's room, else flipped below; never off
        // the top of the window, and never over the pills. (Where the
        // message sits depends on the demo's GIF, made with ffmpeg if found.)
        assert!(viewer.top() >= px(0.), "{viewer:?}");
        assert!(viewer.bottom() <= pills.top() || viewer.top() >= pills.top(), "{viewer:?} vs {pills:?}");
        assert_eq!(viewer.left(), pills.left(), "outgoing: aligned with the pills' left edge");
        let tab = cx.update(|_, cx| view.read(cx).reaction_viewer.clone());
        assert_eq!(tab, Some((plan.id, None)));

        // A tab filters to one emoji.
        let thumbs = bounds(&mut cx, format!("reaction-tab-{}-0", plan.id));
        cx.simulate_click(thumbs.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        let tab = cx.update(|_, cx| view.read(cx).reaction_viewer.clone());
        assert_eq!(tab, Some((plan.id, Some("👍".into()))));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn reactions_sit_on_the_inside_bottom_corner(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let reacted = wait_for_messages(&backend, GROUP, 2, |m| !m.reactions.is_empty());
        let mut cx = open(cx, &backend, GROUP);

        for message in &reacted {
            let bubble = bounds(&mut cx, format!("bubble-{}", message.id));
            let pills = bounds(&mut cx, format!("reactions-{}", message.id));
            // Overlapping the bubble's bottom edge...
            assert!(pills.top() < bubble.bottom() && pills.bottom() > bubble.bottom(), "{pills:?} vs {bubble:?}");
            // ...at the corner facing the middle of the conversation.
            if message.direction == Direction::Outgoing {
                assert!(pills.left() > bubble.left() && pills.left() < bubble.left() + px(20.), "{pills:?} vs {bubble:?}");
            } else {
                assert!(pills.right() < bubble.right() && pills.right() > bubble.right() - px(20.), "{pills:?} vs {bubble:?}");
            }
        }
        backend.shutdown();
    }
}
