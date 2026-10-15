//! Left pane: search box and conversation list.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::assets::IconName;
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Entity, FocusHandle, Focusable, FontStyle, FontWeight, HighlightStyle, ListAlignment, ListOffset, ListState, SharedString, StyledText, Subscription, Task,
    Window, div, list, px,
};
use signal_backend::ConnectionState;
use signal_store::{Conversation, ConversationId, ConversationKind, DirectoryEntry, SearchHit};

use crate::state::{AppEvent, AppState};
use crate::theme::Palette;
use crate::views::common::{StatusPlace, collapse_whitespace, conversation_avatar, short_timestamp, status_icon};

const ROW_HEIGHT: f32 = 72.;
const HEADER_HEIGHT: f32 = 36.;
const ARCHIVE_BUTTON_HEIGHT: f32 = 52.;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
const DRAFT_LABEL: &str = "Draft:";

/// One entry in the conversation list. Like Signal Desktop, "Pinned" and
/// "Chats" headers appear only when something is pinned (and not while
/// filtering), and archived chats sit behind a button at the end.
enum Item {
    Header(&'static str),
    Conversation { conversation: Box<Conversation>, typing: bool },
    ArchiveButton(usize),
    /// Someone or a group from the directory, without a chat yet.
    Directory(DirectoryEntry),
    /// "Find … on Signal" for a username or phone number.
    Find(String),
}

impl Item {
    /// For telling whether the list's layout changed.
    fn kind(&self) -> u8 {
        match self {
            Item::Header(_) => 0,
            Item::Conversation { .. } => 1,
            Item::ArchiveButton(_) => 2,
            Item::Directory(_) => 3,
            Item::Find(_) => 4,
        }
    }

    fn height(&self) -> f32 {
        match self {
            Item::Header(_) => HEADER_HEIGHT,
            Item::ArchiveButton(_) | Item::Find(_) => ARCHIVE_BUTTON_HEIGHT,
            Item::Conversation { .. } | Item::Directory(_) => ROW_HEIGHT,
        }
    }
}

fn italic() -> HighlightStyle {
    HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() }
}

pub struct Sidebar {
    state: Entity<AppState>,
    search: Entity<InputState>,
    query: String,
    hits: Vec<SearchHit>,
    /// Contacts and groups matching the search that have no chat yet.
    directory_hits: Vec<DirectoryEntry>,
    /// A username or phone number being looked up.
    finding: Option<String>,
    search_task: Option<Task<()>>,
    list: ListState,
    items: Rc<Vec<Item>>,
    /// Showing archived chats instead of the inbox.
    archive_open: bool,
    /// The kind of item at each position when the list was last laid out.
    layout_key: Vec<u8>,
    /// Desktop's "Search in chat": the search box searches this chat only.
    chat_search: Option<ChatSearch>,
    /// The message results' scroll position, for their scrollbar.
    hits_scroll: gpui_kit::ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// The search box while it's searching one chat. It has its own input, for
/// its "Search chat" placeholder.
struct ChatSearch {
    conversation: ConversationId,
    input: Entity<InputState>,
    _subscription: Subscription,
}

impl Sidebar {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.set_query(input.read(cx).value().trim().to_owned(), cx);
                }
            }),
            cx.subscribe_in(&state, window, |this, _, event: &AppEvent, window, cx| match event {
                AppEvent::RecipientFound { query, result } => {
                    if this.finding.as_deref() != Some(query.as_str()) {
                        return;
                    }
                    this.finding = None;
                    match result {
                        Ok(entry) => this.open_entry(entry.clone(), window, cx),
                        Err(message) => {
                            let message = message.clone();
                            this.state.update(cx, |state, cx| state.report_error(message, cx));
                            cx.notify();
                        }
                    }
                }
                AppEvent::SearchInChat(conversation) => this.search_in_chat(conversation.clone(), window, cx),
                AppEvent::ConversationsChanged | AppEvent::SelectionChanged | AppEvent::TypingChanged { .. } => {
                    if let AppEvent::SelectionChanged = event {
                        // Searching another chat ends when you leave it.
                        let selected = this.state.read(cx).selected.clone();
                        if this.chat_search.as_ref().is_some_and(|s| Some(&s.conversation) != selected.as_ref()) {
                            this.end_chat_search(window, cx);
                        }
                        this.scroll_to_selected(cx);
                    }
                    cx.notify()
                }
                AppEvent::MessageChanged { .. }
                | AppEvent::MessageRemoved { .. }
                | AppEvent::ConversationCleared(_)
                | AppEvent::JumpTo { .. }
                | AppEvent::PinsChanged(_)
                | AppEvent::AuthorNamesChanged(_)
                | AppEvent::Error(_)
                | AppEvent::LinkPreview { .. }
                | AppEvent::StickerUpload(_)
                | AppEvent::ViewImage(_)
                | AppEvent::Gifs { .. } => {}
            }),
        ];
        Self {
            state,
            search,
            query: String::new(),
            hits: Vec::new(),
            directory_hits: Vec::new(),
            finding: None,
            search_task: None,
            list: ListState::new(0, ListAlignment::Top, px(ROW_HEIGHT * 4.)),
            items: Rc::default(),
            archive_open: false,
            layout_key: Vec::new(),
            chat_search: None,
            hits_scroll: gpui_kit::ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The search box in use: the chat search's while there is one.
    pub fn search_input(&self) -> Entity<InputState> {
        match &self.chat_search {
            Some(search) => search.input.clone(),
            None => self.search.clone(),
        }
    }

    /// A new query: search again, from the top of the list (a long list
    /// otherwise keeps a short one's scroll position, or the reverse).
    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query != self.query {
            self.list.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
            self.hits_scroll.set_offset(gpui_kit::Point::default());
        }
        self.query = query;
        self.run_search(cx);
        cx.notify();
    }

    /// Desktop's "Search in chat": the search box searches `conversation`.
    pub fn search_in_chat(&mut self, conversation: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat_search.as_ref().is_none_or(|s| s.conversation != conversation) {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search chat"));
            let subscription = cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.set_query(input.read(cx).value().trim().to_owned(), cx);
                }
            });
            self.chat_search = Some(ChatSearch { conversation, input, _subscription: subscription });
            self.set_query(String::new(), cx);
        }
        if let Some(search) = &self.chat_search {
            search.input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Back to searching everything (with whatever the main box holds).
    fn end_chat_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat_search.take().is_none() {
            return;
        }
        let query = self.search.read(cx).value().trim().to_owned();
        self.set_query(query, cx);
        self.search.update(cx, |input, cx| input.focus(window, cx));
    }

    fn scroll_to_selected(&self, cx: &App) {
        let Some(selected) = self.state.read(cx).selected.clone() else { return };
        let ix = self
            .items
            .iter()
            .position(|item| matches!(item, Item::Conversation { conversation, .. } if conversation.id == selected));
        if let Some(ix) = ix {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    fn set_archive_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.archive_open = open;
        self.list.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
        cx.notify();
    }

    /// Rebuilds the list entries and keeps the list's item count in step.
    fn rebuild_items(&mut self, cx: &App) {
        let state = self.state.read(cx);
        // Everything was unarchived (on the phone) while the archive was open.
        if self.archive_open && state.archived.is_empty() {
            self.archive_open = false;
        }
        let conversations = if self.chat_search.is_some() && !self.query.is_empty() {
            Vec::new()
        } else {
            self.visible_conversations(cx)
        };
        let pinned = conversations.iter().take_while(|c| c.pinned).count();
        let inbox = self.query.is_empty() && !self.archive_open;
        // Searching one chat lists only its messages (the hits, below).
        let chat_search = self.chat_search.is_some() && !self.query.is_empty();
        let sections = inbox && pinned > 0;
        let archived = if inbox { state.archived.len() } else { 0 };
        let mut items = Vec::with_capacity(conversations.len() + 2);
        for (ix, conversation) in conversations.into_iter().enumerate() {
            if sections && ix == 0 {
                items.push(Item::Header("Pinned"));
            }
            if sections && ix == pinned {
                items.push(Item::Header("Chats"));
            }
            let typing = !state.typing_in(&conversation.id).is_empty();
            items.push(Item::Conversation { conversation: Box::new(conversation.clone()), typing });
        }
        if archived > 0 {
            items.push(Item::ArchiveButton(archived));
        }
        // Searching also finds contacts and groups without a chat, and offers
        // to look up a username or phone number, as Desktop does.
        if !self.query.is_empty() && !self.archive_open && !chat_search {
            let (groups, contacts): (Vec<_>, Vec<_>) =
                self.directory_hits.iter().cloned().partition(|e| e.kind == ConversationKind::Group);
            if !items.is_empty() && (!contacts.is_empty() || !groups.is_empty()) {
                items.insert(0, Item::Header("Chats"));
            }
            for (title, entries) in [("Contacts", contacts), ("Groups", groups)] {
                if !entries.is_empty() {
                    items.push(Item::Header(title));
                    items.extend(entries.into_iter().map(Item::Directory));
                }
            }
            let home = state.account.as_ref().and_then(|a| a.phone.as_deref());
            let lookup = signal_backend::recipients::username_in(&self.query)
                .or_else(|| signal_backend::recipients::phone_in(&self.query, home));
            if let Some(lookup) = lookup {
                items.push(Item::Find(lookup));
            }
        }

        // Rows and headers have fixed heights, so the list only needs
        // remeasuring when the kinds of item at each position change.
        let key: Vec<u8> = items.iter().map(Item::kind).collect();
        if key != self.layout_key {
            let old = self.list.item_count();
            // A splice counts rows added at the top of an empty list as
            // above the visible one and jumps to the end; rows are by
            // position here, so keep the position.
            let top = self.list.logical_scroll_top();
            if items.len() > old {
                self.list.splice(old..old, items.len() - old);
            } else if items.len() < old {
                self.list.splice(items.len()..old, 0);
            }
            self.list.scroll_to(ListOffset {
                item_ix: top.item_ix.min(items.len().saturating_sub(1)),
                offset_in_item: top.offset_in_item,
            });
            self.list.remeasure();
            self.layout_key = key;
        }
        self.items = Rc::new(items);
    }

    fn render_item(&self, ix: usize, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let palette = Palette::get(cx);
        let selected = self.state.read(cx).selected.clone();
        match self.items.get(ix) {
            Some(Item::Header(title)) => div()
                .h(px(HEADER_HEIGHT))
                .px_4()
                .pt_3()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.text_secondary)
                .child(*title)
                .into_any_element(),
            Some(Item::Conversation { conversation, typing }) => {
                let is_selected = selected.as_ref() == Some(&conversation.id);
                self.render_row(conversation, is_selected, *typing, palette, cx).into_any_element()
            }
            Some(Item::Directory(entry)) => self.render_directory_entry(entry, palette, cx).into_any_element(),
            Some(Item::Find(lookup)) => {
                let busy = self.finding.as_deref() == Some(lookup.as_str());
                let query = lookup.clone();
                div()
                    .h(px(ARCHIVE_BUTTON_HEIGHT))
                    .px_2()
                    .py_1()
                    .child(
                        Button::new("find-recipient")
                            .ghost()
                            .size_full()
                            .icon(IconName::Search)
                            .label(if busy { format!("Looking up {lookup}…") } else { format!("Find {lookup} on Signal") })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.finding = Some(query.clone());
                                this.state.read(cx).send(signal_backend::Command::FindRecipient { query: query.clone() });
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            Some(Item::ArchiveButton(count)) => div()
                .h(px(ARCHIVE_BUTTON_HEIGHT))
                .px_2()
                .py_1()
                .child(
                    Button::new("archived-chats")
                        .ghost()
                        .size_full()
                        .label(format!("Archived chats ({count})"))
                        .text_color(palette.text_secondary)
                        .on_click(cx.listener(|this, _, _, cx| this.set_archive_open(true, cx))),
                )
                .into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// Message full-text search, debounced.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        if self.query.chars().count() < 2 {
            self.hits.clear();
            self.directory_hits.clear();
            self.search_task = None;
            return;
        }
        let backend = self.state.read(cx).backend.clone();
        let query = self.query.clone();
        let scope = self.chat_search.as_ref().map(|s| s.conversation.clone());
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let db = backend.db().clone();
            let found = backend
                .spawn(async move {
                    Ok::<_, signal_store::Error>(match scope {
                        // One chat: its messages only.
                        Some(chat) => (db.search_conversation(&chat, &query, 100).await?, Vec::new()),
                        None => (db.search_messages(&query, 50).await?, db.search_directory(&query, 20).await?),
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (hits, directory) = found.unwrap_or_default();
                this.hits = hits;
                this.directory_hits = directory;
                cx.notify();
            });
        }));
    }

    fn visible_conversations<'a>(&self, cx: &'a App) -> Vec<&'a Conversation> {
        let state = self.state.read(cx);
        let query = self.query.to_lowercase();
        let source = if self.archive_open { &state.archived } else { &state.conversations };
        source
            .iter()
            .filter(|c| {
                query.is_empty()
                    || c.title.to_lowercase().contains(&query)
                    || c.phone.as_deref().is_some_and(|p| p.contains(&query))
            })
            .collect()
    }

    fn render_row(&self, conversation: &Conversation, selected: bool, typing: bool, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let id = conversation.id.clone();
        // Signal Desktop italicizes typing indicators and the "Draft:" label.
        let draft = conversation
            .draft
            .as_deref()
            .map(collapse_whitespace)
            .filter(|d| !d.is_empty());
        let has_draft = draft.is_some();
        // Desktop's order: blocked, then someone typing, then the draft.
        let preview = if conversation.blocked {
            StyledText::new("Blocked").into_any_element()
        } else if typing {
            typing_dots(palette.text_secondary).into_any_element()
        } else if let Some(draft) = draft {
            StyledText::new(format!("{DRAFT_LABEL} {draft}"))
                .with_highlights([(0..DRAFT_LABEL.len(), italic())])
                .into_any_element()
        } else {
            StyledText::new(conversation.last_message_preview.clone().unwrap_or_default()).into_any_element()
        };
        let unread = conversation.unread_count;
        let title = match conversation.kind {
            ConversationKind::NoteToSelf => "Note to Self".to_owned(),
            _ => conversation.title.clone(),
        };

        let row = div()
            .id(SharedString::from(format!("conv-{}", conversation.id.0)))
            .debug_selector({
                let id = conversation.id.0.clone();
                move || format!("conv-{id}")
            })
            .size_full()
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded_lg()
            .cursor_pointer()
            .when(selected, |d| d.bg(palette.row_selected))
            .when(!selected, |d| d.hover(|d| d.bg(palette.row_hover)))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _, cx| {
                    let id = id.clone();
                    this.state.update(cx, |state, cx| state.select(Some(id), cx));
                }
            }))
            .child(conversation_avatar(conversation, px(48.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .when(conversation.muted, |d| {
                                d.child(Icon::new(IconName::BellOff).size_3().text_color(palette.text_secondary))
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(palette.text_secondary)
                                    .child(if conversation.last_message_at > 0 {
                                        short_timestamp(conversation.last_message_at)
                                    } else {
                                        String::new()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .line_height(px(18.))
                                    .line_clamp(2)
                                    .text_ellipsis()
                                    .text_color(palette.text_secondary)
                                    .child(preview),
                            )
                            .when_some(
                                conversation.last_outgoing_status.filter(|_| unread == 0 && !typing && !has_draft),
                                |d, status| {
                                    let key = conversation.id.0.clone();
                                    d.when_some(
                                        status_icon(status, palette.text_secondary, palette, StatusPlace::List, key),
                                        |d, icon| d.child(div().flex_none().ml(px(6.)).child(icon)),
                                    )
                                },
                            )
                            // "Mark as unread" with nothing new: a badge without a count.
                            .when(unread == 0 && conversation.marked_unread, |d| {
                                d.child(div().flex_none().size(px(12.)).rounded_full().bg(palette.badge))
                            })
                            .when(unread > 0, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .min_w(px(20.))
                                        .h(px(20.))
                                        .px_1p5()
                                        .rounded_full()
                                        .bg(palette.badge)
                                        .text_color(palette.on_accent)
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(if unread > 999 { "999+".to_string() } else { unread.to_string() }),
                                )
                            }),
                    ),
            );
        let state = self.state.clone();
        let menu_conversation = conversation.clone();
        let row = row.context_menu(move |menu, window, cx| {
            crate::views::chat_menu::build(menu, &menu_conversation, &state, Default::default(), window, cx)
        });
        div().w_full().h(px(ROW_HEIGHT)).px_2().py(px(2.)).child(row)
    }

    fn render_directory_entry(&self, entry: &DirectoryEntry, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let start = entry.clone();
        let avatar = match entry.kind {
            ConversationKind::Group => {
                let (bg, fg) = crate::theme::avatar_colors(crate::theme::color_for(&entry.id.0));
                div()
                    .size(px(48.))
                    .flex_none()
                    .rounded_full()
                    .bg(bg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Users).size_5().text_color(fg))
                    .into_any_element()
            }
            _ => crate::views::common::person_avatar(
                entry.id.as_direct().and_then(|sid| self.state.read(cx).photo_of(sid)),
                &entry.title,
                crate::theme::color_for(&entry.id.0),
                px(48.),
            ),
        };
        div().w_full().h(px(ROW_HEIGHT)).px_2().py(px(2.)).child(
            div()
                .id(SharedString::from(format!("directory-{}", entry.id.0)))
                .size_full()
                .px_3()
                .flex()
                .items_center()
                .gap_3()
                .rounded_lg()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
                .on_click(cx.listener(move |this, _, window, cx| this.open_entry(start.clone(), window, cx)))
                .child(avatar)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(entry.title.clone()))
                        .when_some(entry.phone.clone().filter(|p| *p != entry.title), |d, phone| {
                            d.child(div().text_sm().truncate().text_color(palette.text_secondary).child(phone))
                        }),
                ),
        )
    }

    /// Starts (or opens) the chat and leaves search, like Desktop.
    fn open_entry(&mut self, entry: DirectoryEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| state.start_conversation(entry, cx));
        self.search.update(cx, |search, cx| search.set_value("", window, cx));
        self.query.clear();
        self.run_search(cx);
        cx.notify();
    }

    fn render_hits(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(palette.panel_border)
            .pt_2()
            .child(
                div()
                    .px_4()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.text_secondary)
                    .child("MESSAGES"),
            )
            .children(self.hits.iter().take(if self.chat_search.is_some() { 100 } else { 20 }).map(|hit| {
                // In one chat, who said it; across chats, which chat.
                let title = if self.chat_search.is_some() {
                    match &hit.author {
                        None => "You".to_owned(),
                        Some(_) => hit.author_name.clone().unwrap_or_else(|| "Unknown".into()),
                    }
                } else {
                    state.conversation(&hit.conversation_id).map(|c| c.title.clone()).unwrap_or_default()
                };
                let id = hit.conversation_id.clone();
                let message_id = hit.message_id;
                div()
                    .id(SharedString::from(format!("hit-{}", hit.message_id)))
                    .mx_2()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|d| d.bg(palette.row_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id: ConversationId = id.clone();
                        this.state.update(cx, |state, cx| state.jump_to(id, message_id, cx));
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                            .child(div().text_xs().text_color(palette.text_secondary).child(short_timestamp(hit.sent_at))),
                    )
                    .child(snippet(&hit.snippet, palette))
            }))
    }
}

/// Desktop's typing indicator in the chat list: three 6pt dots, 38pt wide,
/// fading in turn (the list's animation doesn't scale them).
fn typing_dots(color: gpui_kit::Hsla) -> impl IntoElement {
    use gpui_kit::{Animation, AnimationExt as _};
    div()
        .debug_selector(|| "list-typing".into())
        .h(px(18.))
        .flex()
        .items_center()
        .child(div().w(px(38.)).h(px(8.)).px(px(1.)).flex().items_center().justify_between().children((0..3).map(move |ix| {
            div().size(px(6.)).rounded_full().bg(color).with_animation(
                ("list-typing-dot", ix),
                Animation::new(Duration::from_millis(1600)).repeat(),
                move |dot, t| dot.opacity(crate::views::conversation::typing_dot(t, ix).0),
            )
        })))
}

/// Renders an FTS snippet, bolding the `\u{2}…\u{3}` marked matches.
pub fn snippet(text: &str, palette: Palette) -> impl IntoElement {
    let mut row = div().text_sm().text_color(palette.text_secondary).line_clamp(2).flex().flex_wrap();
    let mut rest = text;
    while let Some(start) = rest.find('\u{2}') {
        if start > 0 {
            row = row.child(rest[..start].to_owned());
        }
        let after = &rest[start + 1..];
        let end = after.find('\u{3}').unwrap_or(after.len());
        row = row.child(
            div()
                .font_weight(FontWeight::BOLD)
                .text_color(palette.text)
                .child(after[..end].to_owned()),
        );
        rest = after.get(end + 1..).unwrap_or("");
    }
    if !rest.is_empty() {
        row = row.child(rest.to_owned());
    }
    row
}

impl Focusable for Sidebar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let state = self.state.read(cx);
        let connection = state.connection.clone();
        self.rebuild_items(cx);
        let row_count = self.items.len();
        let search_height: f32 = self.items.iter().map(Item::height).sum();
        let entity = cx.entity();
        let searching = !self.query.is_empty();

        use signal_backend::HistoryProgress;
        let banner: Option<(SharedString, IconName)> = match &state.history {
            Some(HistoryProgress::Done { messages, .. }) => {
                Some((format!("Synced {messages} messages from your phone").into(), IconName::CheckCheck))
            }
            _ => match connection {
                ConnectionState::Offline => Some(("Offline — reconnecting…".into(), IconName::WifiOff)),
                ConnectionState::Connecting => Some(("Connecting…".into(), IconName::Clock)),
                ConnectionState::Syncing => Some(("Loading messages…".into(), IconName::Download)),
                ConnectionState::Online => None,
            },
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.sidebar)
            .text_color(palette.text)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(52.))
                    .pl(px(92.)) // clear the macOS traffic lights
                    .pr_4()
                    .when(self.archive_open, |d| {
                        d.gap_1().child(
                            Button::new("archive-back")
                                .ghost()
                                .small()
                                .icon(IconName::ChevronLeft)
                                .tooltip("Back to chats")
                                .on_click(cx.listener(|this, _, _, cx| this.set_archive_open(false, cx))),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if self.archive_open { "Archived chats" } else { "Chats" }),
                    )
                    .when(!self.archive_open, |d| {
                        d.child(
                            Button::new("new-group-button")
                                .ghost()
                                .small()
                                .icon(IconName::SquarePen)
                                .tooltip("New group")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    crate::views::new_group::open(this.state.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new("settings-button")
                                .ghost()
                                .small()
                                .icon(IconName::Settings)
                                .tooltip("Settings (⌘,)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    crate::views::settings::open(this.state.clone(), window, cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("sidebar-search")
                    .debug_selector(|| "sidebar-search".into())
                    .px_3()
                    .pb_2()
                    .child(
                        match &self.chat_search {
                            // Desktop's look: the chat's avatar and an ✕ that
                            // goes back to searching everything.
                            Some(search) => {
                                let chat = state.conversation(&search.conversation).cloned();
                                Input::new(&search.input)
                                    .cleanable(true)
                                    .prefix(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .children(chat.map(|c| conversation_avatar(&c, px(20.))))
                                            .child(
                                                div()
                                                    .id("end-chat-search")
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(|this, _, window, cx| this.end_chat_search(window, cx)))
                                                    .child(Icon::new(IconName::X).size_4().text_color(palette.text_secondary)),
                                            ),
                                    )
                                    .into_any_element()
                            }
                            None => Input::new(&self.search)
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).size_4().text_color(palette.text_secondary))
                                .into_any_element(),
                        },
                    )
                    // Backspace in an empty "Search chat", or Escape, searches
                    // everything again.
                    .capture_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                        let Some(search) = &this.chat_search else { return };
                        let empty = search.input.read(cx).value().is_empty();
                        let key = event.keystroke.key.as_str();
                        if key == "escape" || (key == "backspace" && empty) {
                            cx.stop_propagation();
                            this.end_chat_search(window, cx);
                        }
                    })),
            )
            .when_some(banner, |d, (text, icon)| {
                d.child(
                    div()
                        .mx_3()
                        .mb_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(palette.row_hover)
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .child(Icon::new(icon).size_4())
                        .child(text),
                )
            })
            .child(
                div()
                    .relative()
                    .min_h_0()
                    .when(!searching, |d| d.flex_1())
                    // Leave room for message hits below the matching chats.
                    .when(searching, |d| d.h(px(search_height.min(ROW_HEIGHT * 6.))))
                    .child(
                        list(self.list.clone(), move |ix, _window, cx| {
                            entity.update(cx, |this, cx| this.render_item(ix, cx))
                        })
                        .size_full(),
                    )
                    .child(Scrollbar::vertical(&self.list).mode(ScrollbarMode::Hover)),
            )
            .when(searching && row_count == 0 && self.hits.is_empty(), |d| {
                d.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(palette.text_secondary)
                        .child(match self.chat_search.as_ref().and_then(|s| state.conversation(&s.conversation)) {
                            Some(chat) => format!("No results for \"{}\" in {}", self.query, chat.title),
                            None => format!("No results for \"{}\"", self.query),
                        }),
                )
            })
            .when(searching && !self.hits.is_empty(), |d| {
                d.child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .child(
                            div()
                                .id("hits")
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.hits_scroll)
                                .child(self.render_hits(palette, cx)),
                        )
                        .child(Scrollbar::vertical(&self.hits_scroll).mode(ScrollbarMode::Hover)),
                )
            })
            .when(searching && self.hits.is_empty(), |d| d.child(div().flex_1()))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, WindowBounds, WindowOptions, px, size};
    use signal_backend::{Backend, Config};

    use super::*;

    #[gpui_kit::test]
    fn typing_shows_animated_dots_in_the_list(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (backend, _events) =
            Backend::start(Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true })
                .unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (window, _sidebar) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx)))
                .unwrap()
        });
        let alice = ConversationId("d:00000000-0000-4000-8000-00000000a11c".into());
        for _ in 0..200 {
            state.update(cx, |state, cx| state.reload_conversations(cx));
            cx.run_until_parked();
            if state.read_with(cx, |state, _| state.conversation(&alice).is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        state.update(cx, |state, cx| {
            state.handle(
                signal_backend::Event::Typing {
                    conversation: alice.clone(),
                    author: alice.as_direct().unwrap().into(),
                    started: true,
                },
                cx,
            )
        });
        cx.run_until_parked();
        let mut cx = gpui_kit::VisualTestContext::from_window(window, cx);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("list-typing").is_some(), "dots in place of the preview");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn archived_chats_open_behind_a_button(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (backend, _events) = Backend::start(Config {
            data_dir: dir.path().to_owned(),
            db_key: [7; 32],
            device_name: "test".into(),
            demo: true,
        })
        .unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (window, sidebar) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx)))
                .unwrap()
        });
        // The demo seeds its database in the background; reload until it's there.
        for _ in 0..200 {
            state.update(cx, |state, cx| state.reload_conversations(cx));
            cx.run_until_parked();
            // Both lists are read separately; wait until Priya is only archived.
            let settled = state.read_with(cx, |state, _| {
                !state.archived.is_empty() && !state.conversations.iter().any(|c| c.title == "Priya Natarajan")
            });
            if settled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let titles = |cx: &mut TestAppContext| {
            sidebar.read_with(cx, |sidebar, cx| {
                sidebar.visible_conversations(cx).iter().map(|c| c.title.clone()).collect::<Vec<_>>()
            })
        };
        assert!(!titles(cx).contains(&"Priya Natarajan".to_owned()));

        cx.update_window(window, |_, window, cx| window.click("archived-chats", cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(titles(cx), ["Priya Natarajan"]);

        cx.update_window(window, |_, window, cx| window.click("archive-back", cx)).unwrap();
        cx.run_until_parked();
        assert!(titles(cx).contains(&"Maya Chen".to_owned()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn search_finds_contacts_without_a_chat(cx: &mut TestAppContext) {
        use gpui_kit::VisualTestContext;
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (backend, _events) = Backend::start(Config {
            data_dir: dir.path().to_owned(),
            db_key: [7; 32],
            device_name: "test".into(),
            demo: true,
        })
        .unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (window, sidebar) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx)))
                .unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        let dana = ConversationId("d:00000000-0000-4000-8000-0000000000d5".into());
        // The demo seeds in the background: search until Dana turns up.
        let mut found = false;
        for _ in 0..300 {
            sidebar.update_in(&mut cx, |sidebar, window, cx| {
                sidebar.search.update(cx, |input, cx| input.set_value("dana", window, cx));
                sidebar.query = "dana".into();
                sidebar.run_search(cx);
            });
            cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
            cx.run_until_parked();
            found = sidebar.read_with(&cx, |s, _| s.directory_hits.iter().any(|e| e.id == dana));
            if found {
                break;
            }
        }
        assert!(found, "Dana wasn't found");
        // Choosing her starts the chat and leaves search.
        sidebar.update_in(&mut cx, |sidebar, window, cx| {
            let entry = sidebar.directory_hits.iter().find(|e| e.id == dana).unwrap().clone();
            sidebar.open_entry(entry, window, cx);
        });
        for _ in 0..200 {
            cx.run_until_parked();
            if state.read_with(&cx, |s, _| s.selected.as_ref() == Some(&dana)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(state.read_with(&cx, |s, _| s.selected.clone()), Some(dana));
        assert!(sidebar.read_with(&cx, |s, _| s.query.is_empty()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn searching_one_chat_lists_its_messages_by_sender(cx: &mut TestAppContext) {
        use gpui_kit::VisualTestContext;
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, _events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (window, sidebar) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx)))
                .unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        let hikes = ConversationId("g:0000000000000000000000000000000000000000000000000000000000000001".into());
        sidebar.update_in(&mut cx, |sidebar, window, cx| sidebar.search_in_chat(hikes.clone(), window, cx));
        // "ridge" is in the hiking group (twice) and nowhere else.
        let mut hits = Vec::new();
        for attempt in 0..300 {
            // Again now and then: the demo seeds in the background.
            if attempt % 50 == 0 {
                sidebar.update_in(&mut cx, |sidebar, window, cx| {
                    let input = sidebar.search_input();
                    input.update(cx, |input, cx| input.set_value("ridge", window, cx));
                    sidebar.set_query("ridge".into(), cx);
                });
            }
            cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
            cx.run_until_parked();
            hits = sidebar.read_with(&cx, |s, _| s.hits.clone());
            if hits.len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(hits.len() >= 2, "{hits:?}");
        assert!(hits.iter().all(|h| h.conversation_id == hikes));
        assert!(hits.iter().any(|h| h.author.is_none()), "ours: shown as You");
        assert!(hits.iter().any(|h| h.author_name.as_deref() == Some("Maya Chen")));
        // No chats or contacts while searching one chat.
        assert!(sidebar.read_with(&cx, |s, _| s.items.is_empty()));

        // Backspace in the empty box searches everything again.
        sidebar.update_in(&mut cx, |sidebar, window, cx| {
            let input = sidebar.search_input();
            input.update(cx, |input, cx| input.set_value("", window, cx));
            sidebar.set_query(String::new(), cx);
        });
        let input = sidebar.read_with(&cx, |s, _| s.search_input());
        cx.update(|window, cx| window.focus(&input.focus_handle(cx), cx));
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert!(sidebar.read_with(&cx, |s, _| s.chat_search.is_none()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_new_query_starts_at_the_top(cx: &mut TestAppContext) {
        use gpui_kit::VisualTestContext;
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, _events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let (window, sidebar) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx)))
                .unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        sidebar.update_in(&mut cx, |sidebar, _, _| {
            sidebar.list.splice(0..0, 20);
            sidebar.list.scroll_to(ListOffset { item_ix: 15, offset_in_item: px(0.) });
        });
        // Select all, delete: back to the whole list, from the top.
        sidebar.update_in(&mut cx, |sidebar, _, cx| {
            sidebar.query = "maya".into();
            sidebar.set_query(String::new(), cx);
        });
        assert_eq!(sidebar.read_with(&cx, |s, _| s.list.logical_scroll_top().item_ix), 0);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn clearing_a_search_returns_to_the_top_of_the_list(cx: &mut TestAppContext) {
        use gpui_kit::VisualTestContext;
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, _events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        // Short, so the chat list scrolls.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(340.), px(300.)) })),
            ..Default::default()
        };
        let (window, sidebar) = cx.update(|cx| {
            let state = state.clone();
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx))).unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        for _ in 0..300 {
            state.update(&mut cx, |s, cx| s.reload_conversations(cx));
            cx.run_until_parked();
            if state.read_with(&cx, |s, _| s.conversations.len() >= 6) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let drawn = |cx: &mut VisualTestContext| {
            let ids: Vec<ConversationId> = state.read_with(cx, |s, _| s.conversations.iter().map(|c| c.id.clone()).collect());
            ids.iter()
                .enumerate()
                .filter_map(|(ix, id)| cx.debug_bounds(Box::leak(format!("conv-{}", id.0).into_boxed_str())).map(|b| (ix, b.top())))
                .collect::<Vec<_>>()
        };
        cx.run_until_parked();
        assert_eq!(drawn(&mut cx).first().map(|(ix, _)| *ix), Some(0), "the list starts at the top");
        let input = sidebar.read_with(&cx, |s, _| s.search_input());
        cx.update(|window, cx| window.focus(&input.focus_handle(cx), cx));
        // Nothing matches: the list empties, then refills on clearing.
        cx.simulate_input("zzzz");
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-a backspace");
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(sidebar.read_with(&cx, |s, _| s.query.clone()), "");
        let top = sidebar.read_with(&cx, |s, _| s.list.logical_scroll_top());
        assert_eq!((top.item_ix, top.offset_in_item), (0, px(0.)), "{top:?}");
        // And on screen: the first chat is drawn, below the search box.
        let search = cx.debug_bounds("sidebar-search").expect("search box drawn");
        let first = drawn(&mut cx).first().copied();
        assert!(matches!(first, Some((0, y)) if y >= search.bottom()), "{first:?}");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn gear_opens_settings_and_unlink_asks_first(cx: &mut TestAppContext) {
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
        let (window, _sidebar) = cx.update(|cx| {
            let state = crate::state::new_state(backend.clone(), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    // Tall enough for the whole Settings dialog.
                    size: size(px(800.), px(900.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Sidebar::new(state, window, cx))).unwrap()
        });
        cx.run_until_parked();

        cx.update_window(window, |_, window, cx| window.click("settings-button", cx)).unwrap();
        cx.run_until_parked();
        assert!(cx.update_window(window, |_, window, cx| window.has_active_dialog(cx)).unwrap());

        // Unlinking opens a confirmation on top rather than unlinking immediately.
        cx.update_window(window, |_, window, cx| window.click("unlink", cx)).unwrap();
        cx.run_until_parked();
        // Two dialogs are stacked: closing the confirmation leaves Settings open.
        let still_open = cx
            .update_window(window, |_, window, cx| {
                window.close_dialog(cx);
                window.has_active_dialog(cx)
            })
            .unwrap();
        assert!(still_open);
        backend.shutdown();
    }
}
