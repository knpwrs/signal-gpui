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
    AttachmentMeta, ConversationId, ConversationKind, DeliveryStatus, Direction, Message, Reaction, ViewOnce,
};

use crate::state::{AppEvent, AppState};
use crate::image_cache::LruImageCache;
use crate::views::image_viewer::ViewedImage;
use crate::theme::{Palette, color_for};
use crate::views::common::{ClosedByClick, avatar, person_avatar, collapse_whitespace, find_links, conversation_avatar, day_label, message_time, same_day, status_icon, StatusPlace};
use crate::views::composer::{Composer, ComposerEvent};
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};
use crate::shortcuts::timeline;

const PAGE: u32 = 60;
/// Consecutive messages from one author within this window are grouped.
const GROUP_WINDOW_MS: i64 = 3 * 60 * 1000;
/// Signal's default reaction set.
const QUICK_REACTIONS: [&str; 6] = ["❤️", "👍", "👎", "😂", "😮", "😢"];
/// The reactions viewer's size (Desktop's `ReactionViewer`: fixed, the
/// list scrolling).
const REACTION_VIEWER_WIDTH: f32 = 320.;
const REACTION_VIEWER_HEIGHT: f32 = 320.;

pub struct ConversationView {
    pub(crate) state: Entity<AppState>,
    pub id: ConversationId,
    pub(crate) messages: Vec<Message>,
    list: ListState,
    composer: Entity<Composer>,
    picker: Entity<EmojiPicker>,
    loading_older: bool,
    has_more: bool,
    hovered: Option<i64>,
    reaction_bar_for: Option<i64>,
    reaction_bar_closed_by: ClosedByClick,
    reaction_picker_for: Option<i64>,
    /// Messages whose spoilers were clicked to show.
    revealed_spoilers: std::collections::HashSet<i64>,
    /// How much of a long message shows after "Read more" (Desktop's
    /// `displayLimit`), by message.
    display_limits: std::collections::HashMap<i64, usize>,
    /// Incoming messages that arrived while scrolled up (shown on the
    /// jump-to-latest button); they're read once scrolled into view.
    unseen: usize,
    /// Messages are only marked read while you can see them.
    window_active: bool,
    /// Decoded photos for this conversation, bounded; freed with the view.
    image_cache: Entity<LruImageCache>,
    /// The reaction viewer: which message, and which emoji tab (`None` = All).
    reaction_viewer: Option<(i64, Option<String>)>,
    reaction_viewer_closed_by: ClosedByClick,
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
    /// For message requests and name collisions: names of the group's
    /// members and of who added us, who's in our address book, the
    /// collisions whose warning was closed, and whether the 1:1 warning was
    /// (Desktop's `hasDismissedDirectContactSpoofingWarning`, for while the
    /// chat is open).
    names: std::collections::HashMap<String, String>,
    system_contacts: std::collections::HashSet<String>,
    /// Who we gave a nickname: their profile name changes offer "Update".
    nicknamed: std::collections::HashSet<String>,
    acknowledged_collisions: std::collections::BTreeMap<String, Vec<String>>,
    spoofing_dismissed: bool,
    /// For a 1:1 chat's hero: what's known about them.
    hero_details: Option<signal_backend::ContactDetails>,
    /// Desktop's targeted message, which the timeline's keys move and the
    /// message shortcuts act on (see the timeline's `impl` below).
    targeted: Option<i64>,
    /// The timeline's own focus (the composer has the chat's), and whether
    /// it has it (the targeted message shows only then, as Desktop's focus
    /// ring does).
    timeline_focus: FocusHandle,
    timeline_focused: bool,
    /// Unread messages when the chat opened, for ⌘J.
    unread_at_open: u32,
    /// Target the newest message once it's loaded (End, from older ones).
    target_newest: bool,
    /// How loaded messages show in their collapse set (`collapse.rs`), by
    /// id; one missing is shown as it is.
    slots: std::collections::HashMap<i64, Slot>,
    /// Collapse sets opened, by their first message.
    expanded: std::collections::HashSet<i64>,
    /// The oldest message unread when the chat was loaded (Desktop's unread
    /// marker): collapse sets break there, and what follows was unseen.
    unseen_from: Option<i64>,
    /// Messages that arrived while the chat was open: unseen when first
    /// shown, so they stay out of a collapse set's fold (Desktop's
    /// `messageCache` of each item's first `isUnseen`).
    fresh: std::collections::HashSet<i64>,
    /// `unseen_from` was placed from the first page loaded.
    marker_placed: bool,
    /// Drawing the edit history's copies of a message (no clicks in their
    /// time lines).
    drawing_history: bool,
    /// Opened at its unread messages: the chat is read once they're loaded
    /// (so the unread line's place comes from them).
    read_after_load: bool,
    /// Messages mentioning us not yet seen, oldest first: those unread when
    /// the chat opened, and those arriving while scrolled up (Desktop's
    /// unread mentions, for its "@" button).
    unread_mentions: Vec<i64>,
    /// Scrolled in the last few seconds: the floating date shows (Desktop's
    /// `hasRecentlyScrolled`), until this task hides it.
    recently_scrolled: Option<gpui_kit::Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// How a message shows in its collapse set.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    /// As it is.
    Shown,
    /// The first message folded behind a set's button, which it carries.
    Head { set: i64, kind: crate::views::collapse::Kind, count: usize, timer: Option<u32> },
    /// Folded behind set `set`'s button (shown when it's open).
    Folded { set: i64 },
}

/// How long a jumped-to message stays highlighted.
const HIGHLIGHT: std::time::Duration = std::time::Duration::from_secs(2);
/// How long the floating date stays after scrolling (Desktop's
/// `hasRecentlyScrolledTimeout`).
const FLOATING_DATE_FOR: std::time::Duration = std::time::Duration::from_secs(3);

/// Where loading a stretch of the chat lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Landing {
    /// On a message, with a little above it, highlighted briefly (a search
    /// result, a quote, a pin).
    Highlight,
    /// With the unread line at the top.
    Unread,
}

/// Desktop's "N Unread Messages" (`icu:unreadMessages`).
fn unread_line(count: u32) -> String {
    format!("{count} Unread Message{}", if count == 1 { "" } else { "s" })
}

/// A scroll-down button's badge: Desktop shows up to 99, then "99+".
fn badge_count(count: usize) -> String {
    if count < 100 { count.to_string() } else { "99+".into() }
}

/// Whether a chat offers calls, as its header's buttons (Desktop's
/// `getConversationCallMode`): `Some(false)` for a 1:1 chat, `Some(true)`
/// for a group, `None` for neither (a message request, a blocked chat, a
/// group we left or that ended, the Signal chat).
pub fn call_kind(chat: &signal_store::Conversation) -> Option<bool> {
    match chat.kind {
        ConversationKind::Direct if !chat.blocked && chat.accepted && !chat.id.is_signal() => Some(false),
        ConversationKind::Group if !chat.left && !chat.blocked && !chat.ended && chat.accepted => Some(true),
        _ => None,
    }
}

impl ConversationView {
    /// The chat's composer.
    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

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
                    this.note_scrolled(cx);
                    cx.notify();
                });
            });
        });

        let timeline_focus = cx.focus_handle();
        let unread_at_open = state.read(cx).conversation(&id).map_or(0, |c| c.unread_count);
        let subscriptions = vec![
            cx.on_focus(&timeline_focus, window, |this, _, cx| {
                this.timeline_focused = true;
                cx.notify();
            }),
            cx.on_blur(&timeline_focus, window, |this, _, cx| {
                this.timeline_focused = false;
                cx.notify();
            }),
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
                AppEvent::ConversationsChanged => {
                    this.read_open_poll_votes(cx);
                    cx.notify()
                }
                AppEvent::AuthorNamesChanged(names) => {
                    this.rename_authors(names, cx);
                    // A nickname may have come or gone.
                    this.load_request_details(cx);
                }
                AppEvent::ProfileChanged(sid) if this.id.as_direct() == Some(sid.as_str()) => this.load_hero(cx),
                AppEvent::ProfileChanged(_) | AppEvent::OwnProfileChanged => cx.notify(),
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
                    if this.ended(cx) {
                        return;
                    }
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
            revealed_spoilers: Default::default(),
            display_limits: Default::default(),
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
            reaction_bar_closed_by: ClosedByClick::default(),
            reaction_picker_for: None,
            reaction_viewer: None,
            reaction_viewer_closed_by: ClosedByClick::default(),
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
            names: Default::default(),
            system_contacts: Default::default(),
            nicknamed: Default::default(),
            acknowledged_collisions: Default::default(),
            spoofing_dismissed: false,
            hero_details: None,
            targeted: None,
            timeline_focus,
            timeline_focused: false,
            unread_at_open,
            target_newest: false,
            slots: Default::default(),
            expanded: Default::default(),
            unseen_from: None,
            fresh: Default::default(),
            marker_placed: false,
            drawing_history: false,
            read_after_load: false,
            unread_mentions: Vec::new(),
            recently_scrolled: None,
            _subscriptions: subscriptions,
        };
        this.load_request_details(cx);
        let jump = this.state.update(cx, |state, _| state.take_jump(&this.id));
        let unread = this.state.read(cx).conversation(&this.id).is_some_and(|c| c.unread_count > 0);
        match jump {
            Some(message_id) => this.load_around(message_id, cx),
            // Read once its unread messages are found.
            None if unread => this.open_at_unread(cx),
            None => this.load_initial(cx),
        }
        if jump.is_some() || !unread {
            this.mark_read(cx);
        }
        this.load_pins(cx);
        // Desktop fetches a person's profile when their chat opens
        // (`throttledGetProfiles`).
        if let Some(sid) = this.id.as_direct().filter(|_| !this.id.is_signal()).map(str::to_owned) {
            this.state.read(cx).send(signal_backend::Command::LoadProfile { service_id: sid });
        }
        this.load_hero(cx);
        this
    }

    /// What message requests and name collisions need from the database:
    /// the group's members' names (and who added us), who's in our address
    /// book, and the collisions already acknowledged.
    fn load_request_details(&mut self, cx: &mut Context<Self>) {
        let chat = self.state.read(cx).conversation(&self.id).cloned();
        let mut people = chat.as_ref().map(|c| c.members.clone()).unwrap_or_default();
        people.extend(chat.as_ref().and_then(|c| c.inviter().map(str::to_owned)));
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let key = crate::views::message_request::acknowledged_key(&self.id);
        let query = backend.spawn(async move {
            Ok::<_, signal_store::Error>((
                db.display_names(&people).await?,
                db.system_contacts().await?,
                db.setting(&key).await?,
                db.nicknamed().await?,
            ))
        });
        cx.spawn(async move |this, cx| {
            let Ok((names, system, acknowledged, nicknamed)) = query.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.names = names.into_iter().collect();
                this.system_contacts = system.into_iter().collect();
                this.nicknamed = nicknamed.into_iter().collect();
                this.acknowledged_collisions = acknowledged.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
    }

    /// What the hero shows about the person this chat is with.
    fn load_hero(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.id.as_direct().filter(|_| !self.id.is_signal()).map(str::to_owned) else { return };
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let read = backend.spawn(async move { signal_backend::contact_details(&db, &sid).await });
        cx.spawn(async move |this, cx| {
            let details = read.await;
            let _ = this.update(cx, |this, cx| {
                this.hero_details = Some(details);
                cx.notify();
            });
        })
        .detach();
    }

    /// Who added or invited us to this group, by name.
    fn added_by_name(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let chat = state.conversation(&self.id)?;
        crate::views::message_request::added_by_name(state, chat, &self.names)
    }

    /// This group's members' names, for collisions: each member's chat
    /// title, else the name the database knows.
    fn member_names(&self, cx: &App) -> Vec<(String, String)> {
        let state = self.state.read(cx);
        let Some(chat) = state.conversation(&self.id) else { return Vec::new() };
        chat.members
            .iter()
            .filter_map(|aci| {
                let name = state
                    .conversation(&ConversationId::direct(aci))
                    .map(|c| c.title.clone())
                    .or_else(|| self.names.get(aci).cloned())?;
                Some((aci.clone(), name))
            })
            .collect()
    }

    /// Desktop's name collision warning under the header: a 1:1 request
    /// with a contact's name, or group members sharing one.
    fn render_spoofing_warning(&self, palette: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::views::message_request as mr;
        let state = self.state.read(cx);
        let chat = state.conversation(&self.id)?.clone();
        match chat.kind {
            ConversationKind::Direct if !self.spoofing_dismissed => {
                let contact = mr::same_name_contact(&chat, state.conversations.iter().chain(&state.archived))?.clone();
                let sid = chat.id.as_direct()?.to_owned();
                let request = mr::ReviewPerson::of(state, &sid, chat.title.clone());
                let contact = mr::ReviewPerson::of(state, contact.id.as_direct()?, contact.title.clone());
                let view = cx.entity().downgrade();
                let review_state = self.state.clone();
                Some(mr::render_warning(
                    mr::SAME_NAME_TEXT.into(),
                    "Review request",
                    move |window, cx| mr::open_direct_review(review_state.clone(), request.clone(), contact.clone(), window, cx),
                    move |_, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.spoofing_dismissed = true;
                            cx.notify();
                        });
                    },
                    palette,
                ))
            }
            ConversationKind::Group if !chat.left => {
                let me = state.account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
                let members = self.member_names(cx);
                let collisions = mr::name_collisions(&members, &me, &self.system_contacts);
                if collisions.is_empty() || !mr::has_unacknowledged(&self.acknowledged_collisions, &collisions) {
                    return None;
                }
                let state = self.state.read(cx);
                let names: std::collections::HashMap<String, String> = members.into_iter().collect();
                let people: Vec<mr::ReviewPerson> = collisions
                    .values()
                    .flatten()
                    .map(|aci| mr::ReviewPerson::of(state, aci, names.get(aci).cloned().unwrap_or_default()))
                    .collect();
                let view = cx.entity().downgrade();
                let (review_state, system, shown) = (self.state.clone(), self.system_contacts.clone(), collisions.clone());
                let admin_of = chat.admins.contains(&me).then(|| chat.id.clone());
                Some(mr::render_warning(
                    mr::group_warning_text(&collisions),
                    "Review members",
                    move |window, cx| {
                        mr::open_group_review(
                            review_state.clone(),
                            admin_of.clone(),
                            shown.clone(),
                            people.clone(),
                            system.clone(),
                            window,
                            cx,
                        )
                    },
                    move |_, cx| {
                        let collisions = collisions.clone();
                        let _ = view.update(cx, |this, cx| this.acknowledge_collisions(collisions, cx));
                    },
                    palette,
                ))
            }
            _ => None,
        }
    }

    /// Closing the group warning (Desktop's
    /// `acknowledgeGroupMemberNameCollisions`): kept, so it shows again
    /// only for someone new.
    fn acknowledge_collisions(&mut self, collisions: std::collections::BTreeMap<String, Vec<String>>, cx: &mut Context<Self>) {
        let Ok(json) = serde_json::to_string(&collisions) else { return };
        self.acknowledged_collisions = collisions;
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let key = crate::views::message_request::acknowledged_key(&self.id);
        let save = backend.spawn(async move { db.set_setting(&key, &json).await });
        cx.spawn(async move |_, _| {
            if let Err(error) = save.await {
                tracing::warn!(%error, "couldn't save the acknowledged name collisions");
            }
        })
        .detach();
        cx.notify();
    }

    /// Desktop's hero, at the start of the chat (not the Signal chat's,
    /// which has its own).
    fn render_hero(&self, cx: &App) -> Option<AnyElement> {
        if self.id.is_signal() {
            return None;
        }
        let conversation = self.state.read(cx).conversation(&self.id)?;
        Some(crate::views::contact::hero(&self.state, conversation, self.hero_details.as_ref(), cx))
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

    /// Shows the message `author` sent at `sent_at` (from a pin notice or a
    /// quote), saying `missing` if we don't have it.
    fn go_to(&mut self, author: Option<String>, sent_at: i64, missing: &'static str, cx: &mut Context<Self>) {
        let backend = self.state.read(cx).backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move { db.find_message(&id, author.as_deref(), sent_at).await });
        cx.spawn(async move |this, cx| {
            let found = query.await.ok().flatten();
            let _ = this.update(cx, |this, cx| match found {
                Some(message) => this.load_around(message.id, cx),
                None => this.state.update(cx, |state, cx| state.report_error(missing.to_owned(), cx)),
            });
        })
        .detach();
    }

    /// "You pinned a message", "Alice set the disappearing message time to
    /// 1 day.": centered, between messages.
    fn render_notice(&self, notice: &signal_store::Notice, show_day: bool, sent_at: i64, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use signal_store::Notice;
        let mut text = crate::views::notices::notice_text(notice, |sid| self.reactor_name(Some(sid), cx));
        match notice {
            // Desktop names them as we saved them when they're in our
            // address book (`getStringForProfileChange`).
            Notice::ProfileNameChanged { who, old_name, new_name } if self.system_contacts.contains(who) => {
                let sender = self.reactor_name(Some(who), cx);
                text = crate::views::notices::profile_change_text(old_name, new_name, Some(&sender));
            }
            // `ChangeNumberNotification` adds its time.
            Notice::NumberChanged { .. } => text = format!("{text} \u{b7} {}", message_time(sent_at)),
            _ => {}
        }
        let icon = crate::views::notices::notice_icon(notice);
        // `ProfileChangeNotification`'s "Update": their nickname, when we gave one.
        let update_nickname = match notice {
            Notice::ProfileNameChanged { who, .. } if self.nicknamed.contains(who) => {
                self.state.read(cx).conversation(&ConversationId::direct(who)).cloned()
            }
            _ => None,
        };
        // "Learn More" on a refreshed session or a delivery issue.
        let learn_more_dialog = match notice {
            Notice::SessionRefreshed => Some(None),
            Notice::DeliveryIssue { who } => Some(Some(self.reactor_name(Some(who), cx))),
            _ => None,
        };
        let in_group = self.id.as_group().is_some();
        let go_to = match notice {
            Notice::Pinned { author, sent_at, .. } => Some((author.clone(), *sent_at, "Go to message", "Pinned message not found")),
            // Desktop's "View poll" (`PollTerminateNotification`).
            Notice::PollEnded { by, sent_at, .. } => Some((by.clone(), *sent_at, "View poll", "Poll not found")),
            _ => None,
        };
        // Desktop's "Call back" / "Call again".
        let call_back = match notice {
            Notice::Call { incoming, video, .. } if self.id.as_direct().is_some() => Some((*incoming, *video)),
            _ => None,
        };
        // Desktop's "Join" on a group call's notice.
        let join = matches!(notice, Notice::GroupCall { .. });
        // Desktop's `MessageRequestResponseNotification` buttons: "Block or
        // Report..." on the accept (unless blocked since), "Learn More"
        // (the safety tips) on a report.
        let chat = self.state.read(cx).conversation(&self.id).cloned();
        let request_options = match notice {
            Notice::MessageRequest { event: signal_store::MessageRequestEvent::Accept, .. } => chat.filter(|c| !c.blocked),
            _ => None,
        };
        let learn_more = matches!(notice, Notice::MessageRequest { event: signal_store::MessageRequestEvent::Spam, .. });
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_1p5()
                            .when_some(icon, |d, icon| d.child(Icon::new(icon).size_4().flex_none().text_color(palette.text_secondary)))
                            .child(div().text_center().child(text)),
                    )
                    .when_some(update_nickname, |d, chat| {
                        let state = self.state.clone();
                        d.child(
                            div().debug_selector(|| "notice-update-nickname".into()).child(
                                Button::new(SharedString::from(format!("notice-nickname-{sent_at}")))
                                    .label("Update")
                                    .ghost()
                                    .on_click(move |_, window, cx| {
                                        let (state, chat) = (state.clone(), chat.clone());
                                        let Some(sid) = chat.id.as_direct().map(str::to_owned) else { return };
                                        let backend = state.read(cx).backend.clone();
                                        let db = backend.db().clone();
                                        let read = backend.spawn(async move { db.contact_nickname(&sid).await });
                                        let window_handle = window.window_handle();
                                        cx.spawn(async move |cx| {
                                            let current = read.await.ok().flatten().unwrap_or_default();
                                            let _ = cx.update_window(window_handle, |_, window, cx| {
                                                crate::views::nickname::open_edit(state, chat, current, window, cx);
                                            });
                                        })
                                        .detach();
                                    }),
                            ),
                        )
                    })
                    .when_some(learn_more_dialog, |d, sender| {
                        d.child(
                            div().debug_selector(|| "notice-learn-more-issue".into()).child(
                                Button::new(SharedString::from(format!("notice-issue-{sent_at}")))
                                    .label("Learn More")
                                    .ghost()
                                    .on_click(move |_, window, cx| match &sender {
                                        Some(sender) => crate::views::notices::open_delivery_issue(sender, in_group, window, cx),
                                        None => crate::views::notices::open_session_refreshed(window, cx),
                                    }),
                            ),
                        )
                    })
                    .when_some(go_to, |d, (author, at, label, missing)| {
                        d.child(
                            Button::new(SharedString::from(format!("notice-go-{sent_at}")))
                                .label(label)
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| this.go_to(author.clone(), at, missing, cx))),
                        )
                    })
                    .when_some(call_back, |d, (incoming, video)| {
                        d.child(
                            Button::new(SharedString::from(format!("notice-call-{sent_at}")))
                                .label(if incoming { "Call back" } else { "Call again" })
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let chat = this.id.clone();
                                    this.state.update(cx, |state, cx| state.open_call_lobby(chat, video, cx));
                                })),
                        )
                    })
                    .when(join, |d| {
                        d.child(
                            Button::new(SharedString::from(format!("notice-join-{sent_at}")))
                                .label("Join")
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let chat = this.id.clone();
                                    this.state.update(cx, |state, cx| state.open_call_lobby(chat, true, cx));
                                })),
                        )
                    })
                    .when_some(request_options, |d, chat| {
                        let (state, added_by) = (self.state.clone(), self.added_by_name(cx));
                        d.child(
                            div().debug_selector(|| "notice-block-or-report".into()).child(
                                Button::new(SharedString::from(format!("notice-request-{sent_at}")))
                                    .label("Block or Report...")
                                    .ghost()
                                    .on_click(move |_, window, cx| {
                                        crate::views::message_request::accepted_options(&state, &chat, added_by.clone(), window, cx)
                                    }),
                            ),
                        )
                    })
                    .when(learn_more, |d| {
                        d.child(
                            div().debug_selector(|| "notice-learn-more".into()).child(
                                Button::new(SharedString::from(format!("notice-learn-{sent_at}")))
                                    .label("Learn More")
                                    .ghost()
                                    .on_click(|_, window, cx| crate::views::message_request::open_safety_tips(window, cx)),
                            ),
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

    /// A collapse set's button (Desktop's `CollapseSetButton`): its icon,
    /// "N group updates" and the like, and an arrow; it opens or closes
    /// the set.
    #[allow(clippy::too_many_arguments)]
    fn render_collapse_button(
        &self,
        set: i64,
        kind: crate::views::collapse::Kind,
        count: usize,
        timer: Option<u32>,
        open: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::views::collapse::{Kind, button_text};
        let group = self.id.as_group().is_some();
        // Desktop's `group`, `message-thread`, `timer` and `phone` symbols.
        let icon = match kind {
            Kind::Updates if group => IconName::Users,
            Kind::Updates => IconName::MessageCircle,
            Kind::Timer(_) => IconName::Timer,
            _ => IconName::Phone,
        };
        let text = button_text(kind, count, group, timer);
        div()
            .w_full()
            .my(px(10.))
            .flex()
            .justify_center()
            .child(
                div()
                    .id(("collapse-set", set as u64))
                    .debug_selector(move || format!("collapse-{set}"))
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_3()
                    .py_1p5()
                    .rounded_full()
                    .bg(palette.input_bg)
                    .hover(|d| d.bg(palette.row_hover))
                    .cursor_pointer()
                    .text_sm()
                    .text_color(palette.text)
                    .child(Icon::new(icon).size_4().text_color(palette.text_secondary))
                    .child(text)
                    .child(Icon::new(if open { IconName::ChevronUp } else { IconName::ChevronDown }).size_4().text_color(palette.text_secondary))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_set(set, cx))),
            )
            .into_any_element()
    }

    /// Desktop's mini player (`MiniPlayer`), under the header while audio
    /// is loaded, whichever chat it's from: play or pause, who sent it, the
    /// time, the rate while it plays, and close, which stops it.
    fn render_mini_player(&self, palette: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let audio = self.state.read(cx).audio.as_ref()?;
        let playing = audio.is_playing();
        let (position, _) = audio.progress();
        let label = format!("{} · {}", audio.title, crate::audio::clock(position));
        let rate = crate::audio::rate_label(audio.rate);
        let state = self.state.clone();
        let button = |id: &'static str| {
            div()
                .id(id)
                .flex_none()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .hover(|d| d.bg(palette.row_hover))
        };
        Some(
            div()
                .id("mini-player")
                .debug_selector(|| "mini-player".into())
                .flex_none()
                .h(px(40.))
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .bg(palette.input_bg)
                .border_b_1()
                .border_color(palette.panel_border)
                .child(
                    button("mini-player-play")
                        .on_click({
                            let state = state.clone();
                            move |_, _, cx| state.update(cx, |state, cx| state.toggle_current_audio(cx))
                        })
                        .child(Icon::new(if playing { IconName::Pause } else { IconName::Play }).size_4().text_color(palette.text)),
                )
                .child(div().flex_1().min_w_0().truncate().text_sm().child(label))
                .when(playing, |d| {
                    d.child(
                        div()
                            .id("mini-player-rate")
                            .flex_none()
                            .px_2()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(Hsla { a: 0.15, ..palette.text })
                            .text_xs()
                            .on_click({
                                let state = state.clone();
                                move |_, _, cx| state.update(cx, |state, cx| state.cycle_audio_rate(cx))
                            })
                            .child(rate),
                    )
                })
                .child(
                    button("mini-player-close")
                        .on_click(move |_, _, cx| state.update(cx, |state, cx| state.close_audio(cx)))
                        .child(Icon::new(IconName::X).size_4().text_color(palette.text_secondary)),
                )
                .into_any_element(),
        )
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
        let preview = if pin.view_once.is_some() {
            // Desktop's `PinnedMessagesBar__MessagePreview__Text--ViewOnceMedia`.
            "View-once media".into()
        } else if !pin.body.is_empty() {
            collapse_whitespace(&signal_store::hide_spoilers(&pin.body, &pin.styles))
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
        if state.conversation(&self.id).is_some_and(|c| c.unread_count > 0 || c.unread_poll_votes) {
            state.send(Command::MarkRead {
                conversation: self.id.clone(),
            });
        }
    }

    /// Someone voted in our poll here while we're looking at the newest
    /// messages: read at once, as Desktop's `markOpenConversationRead`.
    fn read_open_poll_votes(&self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        if self.window_active && self.at_end() && state.conversation(&self.id).is_some_and(|c| c.unread_poll_votes) {
            state.send(Command::MarkRead { conversation: self.id.clone() });
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
        let unread = self.state.read(cx).conversation(&self.id).is_some_and(|c| c.unread_count > 0 || c.unread_poll_votes);
        if unread || self.unseen > 0 {
            // Not gated on the cached count: a message that just arrived may
            // not be counted in the list we hold yet.
            self.state.read(cx).send(Command::MarkRead { conversation: self.id.clone() });
        }
        self.unseen = 0;
        // Everything down to here has been seen, mentions too.
        self.unread_mentions.clear();
    }

    /// Desktop's "@" button: the oldest mention of us not yet seen.
    fn jump_to_mention(&mut self, cx: &mut Context<Self>) {
        if self.unread_mentions.is_empty() {
            return;
        }
        let oldest = self.unread_mentions.remove(0);
        self.load_around(oldest, cx);
    }

    /// Whether a message mentions us.
    fn mentions_us(&self, message: &Message, cx: &App) -> bool {
        let state = self.state.read(cx);
        state.account.as_ref().is_some_and(|account| message.mentions.iter().any(|m| m.aci == account.aci))
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
                        this.start_seen();
                        this.list.reset(this.messages.len());
                        this.relayout(cx);
                        this.list.set_follow_mode(FollowMode::Tail);
                        this.list.scroll_to_end();
                        if std::mem::take(&mut this.target_newest) {
                            this.targeted = this.messages.last().map(|m| m.id);
                        }
                    }
                    Err(e) => tracing::error!("loading messages: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the chat at its oldest unread message, under Desktop's "N Unread
    /// Messages" line (`unseen_from`, placed as the page loads), noting which
    /// unread ones mention us; then it's read (as opening a chat always read
    /// it here).
    fn open_at_unread(&mut self, cx: &mut Context<Self>) {
        self.loading_older = true;
        self.generation += 1;
        let generation = self.generation;
        let state = self.state.read(cx);
        let me = state.account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
        let backend = state.backend.clone();
        let db = backend.db().clone();
        let id = self.id.clone();
        let query = backend.spawn(async move {
            let first = db.first_unread(&id).await?;
            let mentions = db.unread_mentions(&id, &me).await?;
            Ok::<_, signal_store::Error>((first, mentions))
        });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((Some((first, _)), mentions)) => {
                        this.unread_mentions = mentions;
                        this.read_after_load = true;
                        this.load_around_as(first, Landing::Unread, cx);
                    }
                    Ok((None, _)) => {
                        this.load_initial(cx);
                        this.mark_read(cx);
                    }
                    Err(error) => {
                        tracing::error!("finding unread messages: {error}");
                        this.load_initial(cx);
                        this.mark_read(cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Scrolled by hand: the floating date shows for three seconds (Desktop's
    /// `hasRecentlyScrolled`), though not for a scroll at the newest
    /// messages when it wasn't already showing.
    fn note_scrolled(&mut self, cx: &mut Context<Self>) {
        if self.recently_scrolled.is_none() && self.at_end() {
            return;
        }
        self.recently_scrolled = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLOATING_DATE_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.recently_scrolled = None;
                cx.notify();
            });
        }));
    }

    /// Loads the messages around `message_id`, scrolls to it and
    /// highlights it (a search result).
    fn load_around(&mut self, message_id: i64, cx: &mut Context<Self>) {
        self.load_around_as(message_id, Landing::Highlight, cx);
    }

    fn load_around_as(&mut self, message_id: i64, landing: Landing, cx: &mut Context<Self>) {
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
                this.start_seen();
                // The message jumped to is never folded away.
                this.list.reset(this.messages.len());
                this.relayout(cx);
                if let Slot::Head { set, .. } | Slot::Folded { set } = this.slot(message_id) {
                    this.expanded.insert(set);
                    this.relayout(cx);
                }
                this.list.set_follow_mode(FollowMode::Normal);
                if landing == Landing::Unread {
                    // The unread line at the top, as Desktop scrolls it into
                    // view; with little unread, the list stops at the end.
                    this.list.scroll_to(ListOffset { item_ix: ix, offset_in_item: px(0.) });
                    if std::mem::take(&mut this.read_after_load) {
                        this.mark_read(cx);
                    }
                    cx.notify();
                    return;
                }
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
                this.relayout(cx);
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
                this.relayout(cx);
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
                    // The next one too: reactions decide whether it starts a
                    // group (its name and spacing).
                    this.list.remeasure_items(ix..(ix + 2).min(this.messages.len()));
                } else if this.has_newer
                    && this.messages.last().is_some_and(|last| (message.sent_at, message.id) > (last.sent_at, last.id))
                {
                    // Past what's loaded (we jumped back): it's waiting below.
                    if message.direction == Direction::Incoming {
                        this.unseen += 1;
                        if this.mentions_us(&message, cx) {
                            this.unread_mentions.push(message.id);
                        }
                    }
                } else {
                    // New messages are almost always last.
                    // In the store's order (by time, then row).
                    let ix = this.messages.partition_point(|m| (m.sent_at, m.id) < (message.sent_at, message.id));
                    let incoming = message.direction == Direction::Incoming;
                    let mention = incoming && this.mentions_us(&message, cx);
                    let id = message.id;
                    // Before the splice, which forgets the scroll position.
                    let was_at_end = this.at_end();
                    this.fresh.insert(message.id);
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
                            if mention {
                                this.unread_mentions.push(id);
                            }
                        }
                    }
                }
                this.relayout(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// What was unseen as the chat loaded: from its oldest unread message.
    /// Opening the chat marks it read, which may be done before the page
    /// is read: then the count of unread messages at opening places it.
    fn start_seen(&mut self) {
        self.fresh.clear();
        let unread_at_open = if std::mem::replace(&mut self.marker_placed, true) { 0 } else { self.unread_at_open };
        self.unseen_from = self.messages.iter().find(|m| !m.is_read).map(|m| m.id).or_else(|| {
            let incoming = self.messages.iter().filter(|m| m.direction == Direction::Incoming && m.notice.is_none());
            (unread_at_open > 0).then(|| incoming.rev().nth(unread_at_open as usize - 1).map(|m| m.id)).flatten()
        });
    }

    /// Works out the collapse sets again (Desktop's
    /// `mapItemsIntoCollapseSets` and `CollapseSetViewer`), and has the
    /// rows whose part in one changed measured again.
    fn relayout(&mut self, cx: &App) {
        use crate::views::collapse::{self, Kind};
        // The call going on here stays out of sets: in a 1:1 chat its calls,
        // in a group its newest group call.
        let in_call = self.state.read(cx).call.as_ref().is_some_and(|c| c.conversation == self.id);
        let live_group_call = self
            .messages
            .iter()
            .rposition(|m| matches!(m.notice, Some(signal_store::Notice::GroupCall { .. })))
            .filter(|_| in_call && self.id.as_group().is_some());
        let kinds: Vec<Kind> = self
            .messages
            .iter()
            .enumerate()
            .map(|(ix, m)| collapse::kind_of(m, in_call && (self.id.as_group().is_none() || Some(ix) == live_group_call)))
            .collect();
        let days: Vec<i64> = self.messages.iter().map(|m| day_number(m.sent_at)).collect();
        let marker = self.unseen_from.and_then(|id| self.messages.iter().position(|m| m.id == id));
        let unseen = |ix: usize| marker.is_some_and(|m| ix >= m) || self.fresh.contains(&self.messages[ix].id);
        let mut slots = std::collections::HashMap::new();
        for set in collapse::collapse_sets(&kinds, &days, marker) {
            let folded = collapse::folded(&set, unseen);
            if folded == 0 {
                continue;
            }
            let id = self.messages[set.start].id;
            slots.insert(id, Slot::Head { set: id, kind: set.kind, count: folded, timer: set.ending_timer(&kinds) });
            for message in &self.messages[set.start + 1..set.start + folded] {
                slots.insert(message.id, Slot::Folded { set: id });
            }
        }
        let slot_of = |slots: &std::collections::HashMap<i64, Slot>, id: i64| slots.get(&id).copied().unwrap_or(Slot::Shown);
        for (ix, message) in self.messages.iter().enumerate() {
            if slot_of(&self.slots, message.id) != slot_of(&slots, message.id) {
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        self.slots = slots;
    }

    /// How a message shows in its collapse set.
    fn slot(&self, id: i64) -> Slot {
        self.slots.get(&id).copied().unwrap_or(Slot::Shown)
    }

    /// Opens or closes a collapse set.
    fn toggle_set(&mut self, set: i64, cx: &mut Context<Self>) {
        if !self.expanded.remove(&set) {
            self.expanded.insert(set);
        }
        for (ix, message) in self.messages.iter().enumerate() {
            if matches!(self.slot(message.id), Slot::Head { set: s, .. } | Slot::Folded { set: s } if s == set) {
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        cx.notify();
    }

    fn on_message_removed(&mut self, message_id: i64, cx: &mut Context<Self>) {
        let Some(ix) = self.messages.iter().position(|m| m.id == message_id) else { return };
        self.messages.remove(ix);
        self.list.splice(ix..ix + 1, 0);
        if ix > 0 {
            // Grouping of the neighbours may have changed.
            self.list.remeasure_items(ix - 1..(ix + 1).min(self.messages.len()));
        }
        self.relayout(cx);
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
        // As Desktop: no phone number; the name opens their details.
        let subtitle = conversation.and_then(|c| (c.kind == ConversationKind::Group).then(|| "Group".to_owned()));
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
            .child(
                div()
                    .id("chat-details")
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .when_some(conversation.cloned().filter(|c| c.kind != ConversationKind::NoteToSelf && !c.id.is_signal()), |d, chat| {
                        let state = self.state.clone();
                        d.cursor_pointer().on_click(move |_, window, cx| {
                            crate::views::chat_settings::open(state.clone(), chat.clone(), window, cx);
                        })
                    })
                    .child(match conversation {
                        Some(c) if c.kind == ConversationKind::Direct && !c.id.is_signal() => {
                            let badge = c.id.as_direct().and_then(|sid| crate::views::common::badge_image(self.state.read(cx), sid, 32., cx));
                            crate::views::common::with_badge(conversation_avatar(c, px(32.)), 32., badge)
                        }
                        Some(c) => conversation_avatar(c, px(32.)),
                        None => avatar(&title, color, px(32.)).into_any_element(),
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .min_w_0()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                                    .when(conversation.is_some_and(crate::views::common::is_official), |d| {
                                        d.child(crate::views::common::official_badge(palette))
                                    }),
                            )
                            .when_some(subtitle, |d, s| {
                                d.child(div().text_xs().text_color(palette.text_secondary).child(s))
                            }),
                    ),
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
            // Desktop's call buttons: a 1:1 chat's two.
            .when(conversation.and_then(call_kind) == Some(false), |d| {
                use gpui_kit::component::button::ButtonVariants as _;
                let call = |id: &'static str, icon: IconName, tooltip: &'static str, video: bool| {
                    gpui_kit::component::button::Button::new(id).ghost().icon(icon).tooltip(tooltip).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let chat = this.id.clone();
                            this.state.update(cx, |state, cx| state.open_call_lobby(chat, video, cx));
                        },
                    ))
                };
                d.child(call("video-call", IconName::Video, "Start a video call", true))
                    .child(call("voice-call", IconName::Phone, "Start a call", false))
            })
            // A group's: one button, as Desktop's.
            .when(conversation.and_then(call_kind) == Some(true), |d| {
                use gpui_kit::component::button::ButtonVariants as _;
                d.child(
                    gpui_kit::component::button::Button::new("group-call")
                        .ghost()
                        .icon(IconName::Video)
                        .tooltip("Start a group call")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let chat = this.id.clone();
                            this.state.update(cx, |state, cx| state.open_call_lobby(chat, true, cx));
                        })),
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

    /// Desktop's `ConversationHero` for a message request (and Safety tips).
    fn render_item(&mut self, ix: usize, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(message) = self.messages.get(ix).cloned() else {
            return div().into_any_element();
        };
        let prev = ix.checked_sub(1).and_then(|i| self.messages.get(i));
        let next = self.messages.get(ix + 1);
        let is_group = self.state.read(cx).conversation(&self.id).is_some_and(|c| c.kind == ConversationKind::Group);

        let show_day = prev.is_none_or(|p| !same_day(p.sent_at, message.sent_at));
        let targeted = self.timeline_focused && self.targeted == Some(message.id);
        // In a collapse set: folded away, or its button and (open) itself.
        match self.slot(message.id) {
            Slot::Folded { set } if !self.expanded.contains(&set) => return div().into_any_element(),
            Slot::Head { set, kind, count, timer } => {
                let open = self.expanded.contains(&set);
                let button = self.render_collapse_button(set, kind, count, timer, open, palette, cx);
                return div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .when(ix == 0 && !self.has_more, |d| d.children(self.render_hero(cx)))
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
                    .child(button)
                    .when(open, |d| {
                        let notice = message.notice.as_ref().map(|n| self.render_notice(n, false, message.sent_at, palette, cx));
                        d.children(notice)
                    })
                    .into_any_element();
            }
            _ => {}
        }
        if let Some(notice) = &message.notice {
            let notice = self.render_notice(notice, show_day, message.sent_at, palette, cx);
            let notice = div()
                .w_full()
                .flex()
                .flex_col()
                .children(self.render_unread_line(message.id, palette))
                .child(div().w_full().when(targeted, |d| d.bg(palette.row_hover)).child(notice))
                .into_any_element();
            return match (ix == 0 && !self.has_more).then(|| self.render_hero(cx)).flatten() {
                Some(hero) => div().flex().flex_col().w_full().child(hero).child(notice).into_any_element(),
                None => notice,
            };
        }
        // Desktop's `PaymentEventNotification`: a payment activation event
        // shows as a notice.
        if let Some(signal_store::MessageContent::Payment(event)) = message.content.as_ref().filter(|_| !message.deleted)
            && !event.is_notification()
        {
            let sender = message.author.as_ref().map(|a| self.reactor_name(Some(a), cx));
            let chat = self.state.read(cx).conversation(&self.id).map(|c| c.title.clone());
            let text = event.description(sender.as_deref(), chat.as_deref());
            return div()
                .flex()
                .flex_col()
                .w_full()
                .when(show_day, |d| {
                    d.child(div().pt_4().pb_2().flex().justify_center().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child(day_label(message.sent_at)))
                })
                .child(
                    div()
                        .debug_selector(|| format!("payment-event-{}", message.id))
                        .py_2()
                        .px_6()
                        .text_center()
                        .text_sm()
                        .text_color(palette.text_secondary)
                        .child(format!("💳 {text}")),
                )
                .into_any_element();
        }
        let first_in_group = show_day || prev.is_none_or(|p| !same_group(p, &message));
        let last_in_group = next.is_none_or(|n| !same_group(&message, n));
        let outgoing = message.direction == Direction::Outgoing;
        let author_name = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
        let author_color = color_for(message.author.as_deref().unwrap_or(""));
        let author_photo = message.author.as_deref().and_then(|a| self.state.read(cx).photo_of(a)).map(|p| p.to_path_buf());

        let bubble = self.render_bubble(&message, is_group && first_in_group, first_in_group, last_in_group, palette, cx);
        let cta = self.render_cta(&message, palette);

        // The actions stay while the reaction bar they opened is up, as in Desktop.
        let hovered = self.hovered == Some(message.id)
            || self.menu_open_for == Some(message.id)
            || self.reaction_bar_for == Some(message.id);
        let selecting = self.selecting.as_ref().map(|s| s.contains(&message.id));
        // None on release notes: Desktop gives the Signal chat no menu
        // (TimelineMessage's renderMenu), so no reacting or replying.
        let actions = if hovered && !message.deleted && selecting.is_none() && !self.id.is_signal() {
            Some(self.render_hover_actions(&message, palette, cx).into_any_element())
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
            // A search result just jumped to, or the targeted message.
            .when(self.highlight == Some(message_id) || targeted, |d| d.bg(palette.row_hover))
            .when(targeted, |d| d.debug_selector(move || format!("targeted-{message_id}")))
            // Desktop focuses a message clicked (outside select mode),
            // which targets it.
            .when(selecting.is_none(), |d| {
                d.on_mouse_down(
                    gpui_kit::MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        if let Some(ix) = this.messages.iter().position(|m| m.id == message_id) {
                            this.target(ix, window, cx);
                        }
                    }),
                )
            })
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
            // Desktop centers release notes.
            .when(self.id.is_signal(), |d| d.justify_center())
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
                            let avatar = person_avatar(author_photo.as_deref(), &author_name, author_color, px(28.));
                            let badge = message.author.as_deref().and_then(|a| crate::views::common::badge_image(self.state.read(cx), a, 28., cx));
                            let open = message.author.clone().map(|a| (self.state.clone(), a, self.id.clone()));
                            d.child(
                                div()
                                    .id(("avatar", message_id as u64))
                                    .debug_selector(move || format!("avatar-{message_id}"))
                                    .child(crate::views::common::with_badge(avatar, 28., badge))
                                    .when_some(open, |d, (state, sid, group)| {
                                        d.cursor_pointer().on_click(move |_, window, cx| {
                                            crate::views::contact::open_contact(state.clone(), sid.clone(), Some(group.clone()), window, cx);
                                        })
                                    }),
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
                            .when_some(cta, |d, cta| d.child(cta))
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
                    }),
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
            // The start of the Signal chat: Desktop's hero.
            .when(ix == 0 && !self.has_more && self.id.is_signal(), |d| d.child(signal_hero(palette, cx)))
            .when(ix == 0 && !self.has_more, |d| d.children(self.render_hero(cx)))
            .children(self.render_unread_line(message_id, palette))
            .when(show_day, |d| {
                let signal = self.id.is_signal().then(|| signal_colors(cx));
                d.child(
                    div()
                        .pt_4()
                        .pb_2()
                        .flex()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.text_secondary)
                        .child(
                            div()
                                // Desktop's pill in the Signal chat
                                // (TimelineDateHeader).
                                .when_some(signal, |d, colors| {
                                    d.px_2p5().py_1().rounded_full().border_1().border_color(palette.panel_border).bg(colors.background)
                                })
                                .child(day_label(message.sent_at)),
                        ),
                )
            })
            .child(row)
            .when(ix + 1 == self.messages.len(), |d| d.child(div().h_3()))
            .into_any_element()
    }

    /// Desktop's `TimelineFloatingHeader`: the date of the topmost message
    /// shown, over the top of the timeline while scrolling, except at the
    /// very start of the chat.
    fn render_floating_date(&self, palette: Palette) -> Option<AnyElement> {
        self.recently_scrolled.as_ref()?;
        let top = self.list.logical_scroll_top().item_ix;
        if top == 0 && !self.has_more {
            return None;
        }
        let message = self.messages.get(top)?;
        Some(
            div()
                .absolute()
                .top_2()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .debug_selector(|| "floating-date".into())
                        .px_2p5()
                        .py_1p5()
                        .rounded_full()
                        .bg(palette.popover)
                        .shadow_sm()
                        .text_sm()
                        .text_color(palette.text_secondary)
                        .child(day_label(message.sent_at)),
                )
                .into_any_element(),
        )
    }

    /// Desktop's scroll-down buttons (`ScrollDownButton`), shown while
    /// scrolled up: "@" with how many mentions of us haven't been seen,
    /// over the arrow with how many messages arrived meanwhile.
    fn render_scroll_down_buttons(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, icon: IconName, count: usize| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .relative()
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
                .child(Icon::new(icon).size_5().text_color(palette.text))
                .when(count > 0, |d| {
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
                            .child(badge_count(count)),
                    )
                })
        };
        div()
            .absolute()
            .bottom_4()
            .right_4()
            .flex()
            .flex_col()
            .gap_3()
            .when(!self.unread_mentions.is_empty(), |d| {
                d.child(
                    button("jump-to-mention", IconName::AtSign, self.unread_mentions.len())
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Mentions below").build(window, cx))
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_mention(cx))),
                )
            })
            .child(
                // How many arrived while you were scrolled up.
                button("jump-to-latest", IconName::ArrowDown, self.unseen).on_click(cx.listener(|this, _, _, cx| {
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
                })),
            )
    }

    /// Desktop's `LastSeenIndicator` ("3 Unread Messages"), above the oldest
    /// message unread when the chat loaded: the same place the collapse sets
    /// break (`unseen_from`), with how many were unread as it opened.
    fn render_unread_line(&self, message_id: i64, palette: Palette) -> Option<AnyElement> {
        if self.unseen_from != Some(message_id) {
            return None;
        }
        let count = match self.unread_at_open {
            0 => self.messages.iter().filter(|m| !m.is_read).count().max(1) as u32,
            count => count,
        };
        Some(
            div()
                .debug_selector(|| "unread-line".into())
                .w_full()
                .mt_3()
                .mb_1()
                .py_1()
                .flex()
                .justify_center()
                .bg(palette.row_hover)
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.text_secondary)
                .child(unread_line(count))
                .into_any_element(),
        )
    }

    /// What clicks on a bubble's time line do: none while drawing the edit
    /// history's copies.
    fn meta_clicks(&self, message: &Message, cx: &mut Context<Self>) -> MetaClicks {
        if self.drawing_history {
            return MetaClicks::default();
        }
        let edited = (message.edited && !message.deleted).then(|| {
            let (view, current) = (cx.entity().downgrade(), message.clone());
            std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
                crate::views::edit_history::open(view.clone(), current.clone(), window, cx)
            }) as crate::views::chat_menu::MenuAction
        });
        let retry_delete = self.can_retry_delete(message, cx).then(|| {
            let (state, id) = (self.state.clone(), message.id);
            std::rc::Rc::new(move |window: &mut Window, cx: &mut App| confirm_retry_delete(state.clone(), id, window, cx))
                as crate::views::chat_menu::MenuAction
        });
        MetaClicks { edited, retry_delete }
    }

    /// Our delete for everyone of it failed and may be sent again (Desktop's
    /// `canRetryDeleteForEveryone`).
    fn can_retry_delete(&self, message: &Message, cx: &App) -> bool {
        signal_backend::deletes::retry_kind(message, self.delete_chat(cx), now_millis()).is_some()
    }

    /// A message as the edit history shows it (Desktop's
    /// `EditHistoryMessagesModal`): its bubble alone, nothing clickable in
    /// its time line.
    pub(crate) fn render_history_bubble(&mut self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        self.drawing_history = true;
        let bubble = self.render_bubble(message, false, true, true, palette, cx);
        self.drawing_history = false;
        bubble
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
        let clicks = self.meta_clicks(message, cx);
        let outgoing = message.direction == Direction::Outgoing;
        let corners = Corners::for_bubble(outgoing, first, last);
        // Ours take the chat's color (Desktop's chat colors), solid or a
        // gradient; `bg` stands for it where one color is needed.
        let ours = outgoing.then(|| self.state.read(cx).bubble_fill(&self.id));
        let (bg, fg, meta_color) = if let Some(ours) = ours {
            let bg = match ours {
                crate::chat_color::BubbleFill::Solid(color) => color,
                // The time's room in the text is drawn clear on a gradient.
                crate::chat_color::BubbleFill::Gradient(..) => gpui_kit::transparent_black(),
            };
            (bg, palette.bubble_out_text, palette.bubble_out_meta)
        } else if self.id.is_signal() {
            // Desktop's own color for release notes, with white text.
            let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
            (signal_colors(cx).message, white, Hsla { a: 0.8, ..white })
        } else {
            (palette.bubble_in, palette.bubble_in_text, palette.text_secondary)
        };

        let fill: gpui_kit::Background = ours.map_or_else(|| bg.into(), crate::chat_color::BubbleFill::background);
        let jumbo = jumbo_count(message);

        let meta = show_meta.then(|| {
            let color = if jumbo.is_some() { palette.text_secondary } else { meta_color };
            render_meta(message, color, palette, &clicks)
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

        if let Some(view_once) = message.view_once.filter(|_| !message.deleted) {
            return self.render_view_once(message, view_once, show_author, corners, (fill, fg, meta_color), meta.map(IntoElement::into_any_element), palette, cx);
        }

        if let Some(content) = message.content.as_ref().filter(|_| !message.deleted) {
            return self.render_content(message, content, show_author, corners, (fill, fg, meta_color), meta, palette, cx);
        }

        // Stickers stand alone, without a bubble, at Desktop's size.
        if let Some(sticker) = message.attachments.first().filter(|a| a.sticker && !message.deleted) {
            let meta = show_meta.then(|| render_meta(message, palette.text_secondary, palette, &clicks));
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
                // A sticker reply: the quote, in a bubble, above the sticker
                // (Desktop's `isStickerReply`).
                .when_some(message.quote.clone(), |d, quote| {
                    d.child(
                        div()
                            .max_w(px(STICKER_SIZE))
                            .mb_1()
                            .px_2()
                            .py_1p5()
                            .rounded_lg()
                            .bg(fill)
                            .text_color(fg)
                            .child(self.render_quote(message.id, quote, fg, palette, cx)),
                    )
                })
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
        let author_open = message.author.clone().map(|a| (self.state.clone(), a, self.id.clone()));
        // The mini player's title for its audio: who sent it.
        let title = self.reactor_name(message.author.as_deref(), cx);
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
            let photo_message = message.id;
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
                .bg(fill)
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
                                d.child(author_line(author_name, author_color, author_label, author_open))
                            })
                            .when_some(message.quote.clone(), |d, quote| d.child(self.render_quote(message.id, quote, fg, palette, cx))),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("photo-{}", message.id)))
                        .relative()
                        // Photos open in the viewer; GIFs just play.
                        .when_some(viewed.filter(|_| !gif), |d, viewed| {
                            d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                let mut image = viewed.clone();
                                image.in_chat = Some(crate::views::lightbox::InChat::new(this.id.clone(), photo_message, 0));
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
                                    .child(render_meta(message, palette.on_accent, palette, &clicks)),
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
                            .children(rest.iter().enumerate().filter(|(_, a)| !too_big(a)).map(|(ix, a)| {
                                render_attachment(message.id, ix + 1, a, &title, fg, palette, &self.state, cx)
                            }))
                            .children(too_big_label(message))
                            .when(caption, |d| d.child(message_text(message, fg, self.revealed_spoilers.contains(&message.id), self.display_limits.get(&message.id).copied(), spacer.as_deref().map(|s| (s, bg)), cx)))
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
                .bg(fill)
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
                                d.child(author_line(author_name, author_color, author_label, author_open))
                            })
                            .when_some(message.quote.clone(), |d, quote| d.child(self.render_quote(message.id, quote, fg, palette, cx))),
                    )
                })
                .child(render_preview_card(self.state.clone(), message.id, preview, full, (!header).then_some(corners), meta_color))
                .child(
                    div()
                        .relative()
                        .px_3()
                        .pt_1p5()
                        .pb_2()
                        .when(!message.body.is_empty(), |d| {
                            d.child(message_text(message, fg, self.revealed_spoilers.contains(&message.id), self.display_limits.get(&message.id).copied(), spacer.as_deref().map(|s| (s, bg)), cx))
                        })
                        .when_some(meta, |d, m| d.child(place_meta(m, inline_meta))),
                )
                .into_any_element();
        }

        // A poll shows in place of its text (its body is the question).
        let poll = message.poll.as_ref().filter(|_| !message.deleted).map(|poll| {
            let can_vote = !self.id.is_signal()
                && self.state.read(cx).conversation(&self.id).is_none_or(|c| !c.left && !c.blocked && !c.ended && c.accepted);
            let secondary = if outgoing { meta_color } else { palette.text_secondary };
            crate::views::poll::render_poll(message, poll, outgoing, fg, secondary, can_vote, palette, cx.entity().downgrade(), self.state.clone())
        });
        // The time shares the last line of text when it fits, as in Desktop.
        let inline_meta = meta.is_some() && !message.deleted && !message.body.is_empty() && poll.is_none();
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
            .bg(fill)
            .text_color(fg)
            .when(show_author && !outgoing, |d| {
                d.child(author_line(author_name, author_color, author_label, author_open))
            })
            .when_some(message.quote.clone(), |d, quote| d.child(self.render_quote(message.id, quote, fg, palette, cx)))
            .children(
                message
                    .attachments
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| !too_big(a))
                    .map(|(ix, a)| render_attachment(message.id, ix, a, &title, fg, palette, &self.state, cx)),
            )
            .children((!message.deleted).then(|| too_big_label(message)).flatten())
            .map(|d| {
                if message.deleted {
                    d.child(div().italic().child(self.deleted_text(message, cx)))
                } else if let Some(poll) = poll {
                    d.child(poll)
                } else if !message.body.is_empty() {
                    d.child(message_text(message, fg, self.revealed_spoilers.contains(&message.id), self.display_limits.get(&message.id).copied(), spacer.as_deref().map(|s| (s, bg)), cx))
                } else {
                    d
                }
            })
            .when_some(meta, |d, m| d.child(place_meta(m, inline_meta)))
            .into_any_element()
    }

    /// A shared contact, payment or gift badge in its bubble, with the
    /// author and quote above as for text.
    #[allow(clippy::too_many_arguments)]
    fn render_content(
        &self,
        message: &Message,
        content: &signal_store::MessageContent,
        show_author: bool,
        corners: Corners,
        (bg, fg, meta_color): (gpui_kit::Background, Hsla, Hsla),
        meta: Option<impl IntoElement>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::views::message_content as mc;
        use signal_store::MessageContent;
        let outgoing = message.direction == Direction::Outgoing;
        let secondary = if outgoing { meta_color } else { palette.text_secondary };
        let chat = self.state.read(cx).conversation(&self.id).map(|c| c.title.clone());
        let body = match content {
            MessageContent::Contact(contact) => mc::render_contact(message.id, contact, outgoing, fg, secondary, &self.state, cx),
            MessageContent::Payment(event) => {
                let sender = message.author.as_ref().map(|a| self.reactor_name(Some(a), cx));
                let note = match event {
                    signal_store::PaymentEvent::Notification { note } => note.clone(),
                    _ => None,
                };
                mc::render_payment(event.description(sender.as_deref(), chat.as_deref()), note, fg, secondary)
            }
            MessageContent::GiftBadge(gift) => {
                mc::render_gift_badge(message.id, gift, outgoing, chat.unwrap_or_default(), fg, secondary, &self.state, cx)
            }
        };
        let author_name = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
        let author_open = message.author.clone().map(|a| (self.state.clone(), a, self.id.clone()));
        let author_color = {
            let members = self.state.read(cx).conversation(&self.id).map(|c| c.members.clone()).unwrap_or_default();
            crate::theme::name_color(&members, message.author.as_deref().unwrap_or(""), gpui_kit::component::ActiveTheme::theme(&**cx).is_dark())
        };
        let author_label = message
            .author
            .as_deref()
            .and_then(|author| self.state.read(cx).conversation(&self.id)?.member_labels.get(author).cloned());
        corners
            .apply(div())
            .debug_selector(|| format!("bubble-{}", message.id))
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(bg)
            .text_color(fg)
            .when(show_author && !outgoing, |d| d.child(author_line(author_name, author_color, author_label, author_open)))
            .when_some(message.quote.clone(), |d, quote| d.child(self.render_quote(message.id, quote, fg, palette, cx)))
            .child(body)
            .when_some(meta, |d, m| d.child(place_meta(m, false)))
            .into_any_element()
    }

    /// A view-once message (Desktop's `renderTapToView`): what it is and
    /// where it is in its one viewing, never the media itself. Theirs,
    /// downloaded and not yet viewed, opens on click; not downloaded, the
    /// click downloads it.
    #[allow(clippy::too_many_arguments)]
    fn render_view_once(
        &self,
        message: &Message,
        view_once: ViewOnce,
        show_author: bool,
        corners: Corners,
        (bg, fg, meta_color): (gpui_kit::Background, Hsla, Hsla),
        meta: Option<AnyElement>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let outgoing = message.direction == Direction::Outgoing;
        let first = message.attachments.first();
        let (icon, title, detail) = view_once_label(view_once, outgoing, first);
        let id = message.id;
        let click = match (view_once, outgoing, first) {
            (ViewOnce::Ready, false, Some(a)) if a.local_path.is_some() => Some(Command::OpenViewOnce { message_id: id }),
            (ViewOnce::Ready, false, Some(_)) => Some(Command::FetchAttachment { message_id: id, index: 0 }),
            _ => None,
        };
        let author_name = message.author_name.clone().unwrap_or_else(|| "Unknown".into());
        let author_open = message.author.clone().map(|a| (self.state.clone(), a, self.id.clone()));
        let author_color = {
            let members = self.state.read(cx).conversation(&self.id).map(|c| c.members.clone()).unwrap_or_default();
            crate::theme::name_color(&members, message.author.as_deref().unwrap_or(""), gpui_kit::component::ActiveTheme::theme(&**cx).is_dark())
        };
        let author_label = message
            .author
            .as_deref()
            .and_then(|author| self.state.read(cx).conversation(&self.id)?.member_labels.get(author).cloned());
        let secondary = if outgoing { meta_color } else { palette.text_secondary };
        corners
            .apply(div())
            .id(SharedString::from(format!("view-once-{id}")))
            .debug_selector(move || format!("bubble-{id}"))
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(bg)
            .text_color(fg)
            .when(show_author && !outgoing, |d| d.child(author_line(author_name, author_color, author_label, author_open)))
            .child(
                div()
                    .debug_selector(move || format!("view-once-label-{id}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(icon).size_5().text_color(if click.is_some() { fg } else { secondary }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                            .when_some(detail, |d, detail| d.child(div().text_xs().text_color(secondary).child(detail))),
                    ),
            )
            .when_some(meta, |d, m| d.child(m))
            .when_some(click, |d, command| {
                d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.state.read(cx).send(command.clone())))
            })
            // Desktop's toasts for one that can't open again.
            .when_some(view_once_toast(view_once, outgoing), |d, toast| {
                d.on_click(move |_, window, cx| {
                    use gpui_kit::component::WindowExt as _;
                    window.push_notification(gpui_kit::component::notification::Notification::new().message(toast), cx);
                })
            })
            .into_any_element()
    }

    /// A release note's button (Desktop's `MessageCta`), under it. Desktop's
    /// "donate" opens its donation screen, which this app doesn't have, so
    /// it opens Signal's donation page instead. Other buttons aren't shown,
    /// as in Desktop (`isReleaseNoteCtaId`).
    fn render_cta(&self, message: &Message, palette: Palette) -> Option<AnyElement> {
        let cta = message.cta.as_ref().filter(|c| c.id == "donate" && self.id.is_signal())?;
        let id = message.id;
        Some(
            div()
                .id(("cta", id as u64))
                .debug_selector(move || format!("cta-{id}"))
                .mt_1()
                .px_4()
                .py_1p5()
                .flex()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .bg(palette.accent)
                .text_color(palette.on_accent)
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .on_click(|_, _, cx| cx.open_url(DONATE_URL))
                .child(cta.text.clone().unwrap_or_else(|| "Donate".into()))
                .into_any_element(),
        )
    }

    fn render_hover_actions(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let id = message.id;
        // Desktop's `canReplyOrReact`: not in an ended group, a blocked
        // chat or a message request.
        let ended = self.ended(cx) || self.state.read(cx).conversation(&self.id).is_some_and(|c| c.blocked || !c.accepted);
        let for_reply = message.clone();
        // View-once media can't be saved (Desktop's `canDownload`).
        let downloadable: Vec<AttachmentMeta> = message
            .attachments
            .iter()
            .filter(|a| a.local_path.is_some() && !a.sticker && message.view_once.is_none())
            .cloned()
            .collect();
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
            .when(!ended, |d| d.child(
                div()
                    .relative()
                    .debug_selector(move || format!("react-{id}"))
                    .child(button("react", IconName::FaceSlightlySmilingPlus).on_click(cx.listener(
                        move |this, click, _, cx| {
                            if this.reaction_bar_closed_by.take(click) {
                                return;
                            }
                            this.reaction_bar_for = if this.reaction_bar_for == Some(id) { None } else { Some(id) };
                            cx.notify();
                        },
                    )))
                    .when(self.reaction_bar_for == Some(id), |d| {
                        // Desktop (TimelineMessage): a popper centered above
                        // the react button, 4px off it, kept inside the window.
                        d.child(div().absolute().top_0().left(px(14.)).child(
                            deferred(
                                anchored()
                                    .anchor(Anchor::BottomCenter)
                                    .offset(gpui_kit::point(px(0.), px(-4.)))
                                    .snap_to_window_with_margin(px(8.))
                                    .child(self.render_reaction_bar(message, palette, cx)),
                            )
                            .with_priority(1),
                        ))
                    }),
            ))
            .when(!downloadable.is_empty(), |d| {
                d.child(button("download", IconName::Download).on_click(move |_, _, cx: &mut App| {
                    download_attachments(downloadable.clone(), cx);
                }))
            })
            .when(!ended, |d| {
                d.child(button("reply", IconName::Reply).on_click(cx.listener(move |this, _, window, cx| {
                    let message = for_reply.clone();
                    this.composer.update(cx, |composer, cx| composer.reply_to(message, window, cx));
                })))
            })
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
        let everyone = everyone_delete(&chosen, self.delete_chat(cx), now_millis());
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
                        confirm_delete(this.state.clone(), ids, note_to_self, everyone, window, cx);
                        this.selecting = None;
                        cx.notify();
                    })),
            )
    }

    fn note_to_self(&self, cx: &App) -> bool {
        self.state.read(cx).account.as_ref().is_some_and(|a| self.id.as_direct() == Some(a.aci.as_str()))
    }

    /// What the delete-for-everyone rules need to know about this chat.
    fn delete_chat(&self, cx: &App) -> signal_backend::deletes::Chat {
        let state = self.state.read(cx);
        let me = state.account.as_ref().map(|a| a.aci.as_str());
        let we_are_admin = state
            .conversation(&self.id)
            .is_some_and(|c| !c.left && me.is_some_and(|me| c.admins.iter().any(|a| a == me)));
        signal_backend::deletes::Chat::new(self.note_to_self(cx), self.id.as_group().is_some(), we_are_admin, self.ended(cx))
    }

    /// An admin ended this group (Desktop's `terminated`): no sends,
    /// replies, reactions, edits, pins, votes or calls.
    fn ended(&self, cx: &App) -> bool {
        self.state.read(cx).conversation(&self.id).is_some_and(|c| c.ended)
    }

    /// What a deleted message says, in Desktop's words (`Message.dom.tsx`):
    /// a group admin's delete names the admin.
    fn deleted_text(&self, message: &Message, cx: &App) -> String {
        match &message.deleted_by_admin {
            Some(signal_store::DeletedByAdmin { by: Some(admin) }) => {
                format!("Admin {} deleted this message.", self.reactor_name(Some(admin), cx))
            }
            Some(signal_store::DeletedByAdmin { by: None }) => "You deleted this message.".into(),
            None if message.direction == Direction::Outgoing => "You deleted this message.".into(),
            None => "This message was deleted.".into(),
        }
    }

    fn render_reaction_bar(&self, message: &Message, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::{Animation, AnimationExt as _};
        let id = message.id;
        // Desktop (stylesheets/components/ReactionPickerPicker.scss): the
        // bar fades in while each button rises 24px and fades in, 10ms after
        // the one before, all over 400ms of ease-out-expo.
        let appear = |button: gpui_kit::Stateful<gpui_kit::Div>, ix: usize| {
            button.relative().with_animation(
                ("reaction-appear", ix),
                Animation::new(std::time::Duration::from_millis(REACTION_APPEAR_MS + REACTION_STAGGER_MS * ix as u64)),
                move |button, t| {
                    let p = reaction_appear(t, ix);
                    button.opacity(p).top(px(24. * (1. - p)))
                },
            )
        };
        let mine = message.reactions.iter().find(|r| r.author.is_none()).map(|r| r.emoji.clone());
        let tone = self.picker.read(cx).skin_tone();
        div()
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
            .debug_selector(move || format!("reaction-bar-{id}"))
            .on_mouse_down_out(cx.listener(|this, event, _, cx| {
                this.reaction_bar_closed_by.record(event);
                this.reaction_bar_for = None;
                cx.notify();
            }))
            .children(QUICK_REACTIONS.iter().enumerate().map(|(ix, base)| {
                let glyph = signal_emoji::by_glyph(base)
                    .map_or(*base, |(e, _)| e.glyph_with(tone))
                    .to_owned();
                let selected = mine.as_deref() == Some(glyph.as_str());
                let emoji = glyph.clone();
                let button = div()
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
                    .child(glyph);
                appear(button, ix)
            }))
            .child(appear(
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
                QUICK_REACTIONS.len(),
            ))
            // Keyed by message so moving the bar to another message replays it.
            .with_animation(
                ("reaction-bar-appear", id as usize),
                Animation::new(std::time::Duration::from_millis(REACTION_APPEAR_MS)).with_easing(ease_out_expo),
                |bar, p| bar.opacity(p),
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
            cx.listener(move |this: &mut Self, click: &gpui_kit::ClickEvent, _, cx| {
                if this.reaction_viewer_closed_by.take(click) {
                    return;
                }
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
                .flex_none()
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
            // Desktop's is one size whatever it holds; the list scrolls.
            .w(px(REACTION_VIEWER_WIDTH))
            .h(px(REACTION_VIEWER_HEIGHT))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(palette.popover)
            .border_1()
            .border_color(palette.panel_border)
            .shadow_lg()
            .text_color(palette.text)
            .on_mouse_down_out(cx.listener(|this, event, _, cx| {
                this.reaction_viewer_closed_by.record(event);
                this.reaction_viewer = None;
                cx.notify();
            }))
            .child(
                div()
                    .id(SharedString::from(format!("reaction-tabs-{id}")))
                    .flex_none()
                    .flex()
                    .gap_1()
                    .p_2()
                    // One row of tabs, scrolling sideways when there are many.
                    .overflow_x_scroll()
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
                    .flex_1()
                    .min_h_0()
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
    pub(crate) fn reactor_name(&self, author: Option<&str>, cx: &App) -> String {
        let Some(author) = author else { return "You".into() };
        let state = self.state.read(cx);
        state
            .conversation(&ConversationId::direct(author))
            .map(|c| c.title.clone())
            .or_else(|| self.names.get(author).cloned())
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

/// Desktop's `icu:partiallySent`, in place of the time and status icon
/// (`MessageMetadata`); the message's menu has its details.
const PARTIALLY_SENT: &str = "Partially sent, click for details";

/// Desktop's `icu:partiallyDeleted--clickForDetails`: a delete for
/// everyone that reached only some (the message's menu has its details).
const PARTIALLY_DELETED: &str = "Partially deleted, click for details";

/// What shows in place of the time: Desktop's `icu:partiallySent`, and
/// `icu:sendPaused` for a send held for a captcha.
fn status_words(status: DeliveryStatus) -> Option<&'static str> {
    match status {
        DeliveryStatus::PartialSent => Some(PARTIALLY_SENT),
        DeliveryStatus::Paused => Some("Send paused"),
        _ => None,
    }
}

/// What clicking parts of a bubble's time line does (nothing, in the edit
/// history).
#[derive(Clone, Default)]
struct MetaClicks {
    /// "Edited": Desktop's edit history (`showEditHistoryModal`).
    edited: Option<crate::views::chat_menu::MenuAction>,
    /// "Delete failed, click for details": Desktop's retry dialog.
    retry_delete: Option<crate::views::chat_menu::MenuAction>,
}

/// Desktop's dialog for a delete for everyone that failed
/// (`MessageMetadata`'s `ConfirmationType.DeleteError`).
fn confirm_retry_delete(state: Entity<AppState>, message_id: i64, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .description("Message failed to delete. Check your connection and try again.")
            .confirm()
            .ok_text("Try again")
            .on_ok(move |_, window, cx| {
                state.read(cx).send(Command::RetryDeleteForEveryone { message_id });
                window.close_all_dialogs(cx);
                true
            })
    });
}

fn render_meta(message: &Message, color: Hsla, palette: Palette, clicks: &MetaClicks) -> AnyElement {
    let outgoing = message.direction == Direction::Outgoing;
    if let Some(words) = (outgoing && !message.deleted).then(|| status_words(message.status)).flatten() {
        return div()
            .debug_selector(move || format!("status-{words}"))
            .text_size(px(META_SIZE))
            .text_color(palette.danger)
            .child(words)
            .into_any_element();
    }
    let status = outgoing
        .then(|| status_icon(message.status, color, palette, StatusPlace::Bubble, message.id.to_string()))
        .flatten();
    let id = message.id;
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(META_GAP))
        .text_size(px(META_SIZE))
        .text_color(color)
        .when(message.edited, |d| match clicks.edited.clone() {
            Some(open) => d.child(
                div()
                    .id(SharedString::from(format!("edited-{id}")))
                    .debug_selector(move || format!("edited-{id}"))
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        open(window, cx)
                    })
                    .child("Edited ·"),
            ),
            None => d.child("Edited ·"),
        })
        .when(message.expires_in > 0, |d| d.child(Icon::new(IconName::Timer).size_3().text_color(color)))
        .when(message.status == DeliveryStatus::Failed && outgoing && !message.deleted, |d| {
            d.child(div().text_color(palette.danger).child("Not sent"))
        })
        // Deleted, the status is our delete's (Desktop's
        // `getMessagePropStatus`); a failed one says so in place of the time,
        // and one that reached only some says Desktop's "Partially deleted"
        // (its details show who it missed, with a retry).
        .map(|d| match (message.deleted && message.status == DeliveryStatus::Failed, clicks.retry_delete.clone()) {
            _ if message.deleted && message.status == DeliveryStatus::PartialSent => d.child(
                div().debug_selector(move || format!("delete-partial-{id}")).text_color(palette.danger).child(PARTIALLY_DELETED),
            ),
            (true, Some(retry)) => d.child(
                div()
                    .id(SharedString::from(format!("delete-failed-{id}")))
                    .debug_selector(move || format!("delete-failed-{id}"))
                    .text_color(palette.danger)
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        retry(window, cx)
                    })
                    .child("Delete failed, click for details"),
            ),
            (true, None) => d.child(div().text_color(palette.danger).child("Delete failed")),
            (false, _) => d.child(message_time(message.sent_at)),
        })
        // Desktop leaves 6pt before the status icon.
        .when_some(status, |d, s| d.child(div().ml(px(STATUS_GAP)).child(s)))
        .into_any_element()
}

const BUBBLE_RADIUS: f32 = 18.;
/// Signal Desktop draws stickers up to 200pt.
const STICKER_SIZE: f32 = 200.;

/// A downloaded preview image big enough to span the bubble (Desktop's
/// `shouldUseFullSizeLinkPreviewImage`: both sides ≥ 200 and not roughly
/// square), with its path and display size.
fn full_size_preview_image(preview: &signal_store::LinkPreview) -> Option<(String, (f32, f32))> {
    // A sticker pack's cover stays small (Desktop's
    // `shouldUseFullSizeLinkPreviewImage`).
    if signal_backend::stickers::is_sticker_pack_link(&preview.url) {
        return None;
    }
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
    state: Entity<AppState>,
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
        .on_click(move |_, _, cx| open_link(&state, &url, cx))
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
/// A group message's author, as Desktop shows it: their name in their
/// color, opening the contact modal (`open`: the app, their service id and
/// the group), and their member label.
fn author_line(
    name: String,
    color: Hsla,
    label: Option<signal_store::MemberLabel>,
    open: Option<(Entity<AppState>, String, ConversationId)>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .min_w_0()
        .child(
            div()
                .id("author-name")
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .truncate()
                .child(name)
                .when_some(open, |d, (state, sid, group)| {
                    d.cursor_pointer().on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        crate::views::contact::open_contact(state.clone(), sid.clone(), Some(group.clone()), window, cx);
                    })
                }),
        )
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

/// Length of the reaction bar's entrance and the delay between its buttons
/// (Desktop's `animation-duration` and `animation-delay`).
const REACTION_APPEAR_MS: u64 = 400;
const REACTION_STAGGER_MS: u64 = 10;

/// Eased progress of quick-reaction button `ix`, given progress `t` through
/// its animation, which runs `ix` stagger delays longer than the others so
/// it can wait them out first. GPUI animations have no delay of their own.
pub(crate) fn reaction_appear(t: f32, ix: usize) -> f32 {
    let delay = (REACTION_STAGGER_MS * ix as u64) as f32;
    let elapsed = t * (REACTION_APPEAR_MS as f32 + delay);
    ease_out_expo(((elapsed - delay) / REACTION_APPEAR_MS as f32).clamp(0., 1.))
}

/// Desktop's `$ease-out-expo` (stylesheets/_variables.scss),
/// `cubic-bezier(0.19, 1, 0.22, 1)`.
pub(crate) fn ease_out_expo(x: f32) -> f32 {
    cubic_bezier(0.19, 1., 0.22, 1., x)
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` at `x`: find the curve parameter whose
/// x is `x` by bisection (x is monotonic for x1, x2 in 0..=1), then take its y.
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0. || x >= 1. {
        return x.clamp(0., 1.);
    }
    let at = |a: f32, b: f32, s: f32| 3. * a * s * (1. - s).powi(2) + 3. * b * s * s * (1. - s) + s.powi(3);
    let (mut lo, mut hi) = (0f32, 1f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.;
        if at(x1, x2, mid) < x { lo = mid } else { hi = mid }
    }
    at(y1, y2, (lo + hi) / 2.)
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

/// Whether `newer` continues `older`'s group (one sender, a name and
/// avatar for the run). As Desktop (`areMessagesInSameGroup`): reactions
/// end a group, so the reacted message keeps its time and avatar.
fn same_group(older: &Message, newer: &Message) -> bool {
    older.notice.is_none()
        && newer.notice.is_none()
        && older.reactions.is_empty()
        && older.direction == newer.direction
        && older.author == newer.author
        && (newer.sent_at - older.sent_at).abs() < GROUP_WINDOW_MS
        && same_day(older.sent_at, newer.sent_at)
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
    if let Some(words) = (outgoing && !message.deleted).then(|| status_words(message.status)).flatten() {
        return text_width(cx, words, META_SIZE);
    }
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
        let icon = if matches!(message.status, DeliveryStatus::Delivered | DeliveryStatus::Read | DeliveryStatus::Viewed) { 18. } else { 12. };
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

/// What a click on a stretch of message text does.
#[derive(Debug, Clone, PartialEq)]
enum TextClick {
    Open(String),
    /// Shows the message's spoilers.
    Reveal,
    /// Tries the long message's whole text again.
    DownloadText,
    /// Shows more of a long message.
    ReadMore,
}

/// Desktop's "Read more" (`ts/components/conversation/MessageBodyReadMore.dom.tsx`):
/// a message shows its first 800 characters, and each click shows 3,000
/// more. One at most 100 past the limit shows whole.
const READ_MORE_INITIAL: usize = 800;
const READ_MORE_INCREMENT: usize = 3000;
const READ_MORE_BUFFER: usize = 100;

/// Where `body` is cut to show `limit` characters, if it is (Desktop's
/// `graphemeAndLinkAwareSlice`): at the first grapheme that starts at or
/// past the limit, or past a link that would be cut. Characters are UTF-16
/// code units, as in Desktop; the cut is a byte offset.
fn read_more_cut(body: &str, limit: usize) -> Option<usize> {
    use unicode_segmentation::UnicodeSegmentation as _;
    let utf16 = |text: &str| text.encode_utf16().count();
    if utf16(body) <= limit + READ_MORE_BUFFER {
        return None;
    }
    let mut at = 0;
    let mut starts = body.grapheme_indices(true).map(|(byte, grapheme)| {
        let start = (byte, at);
        at += utf16(grapheme);
        start
    });
    let (cut, _) = starts.find(|(_, units)| *units >= limit)?;
    // Desktop only cuts there if another grapheme starts after it.
    starts.next()?;
    let cut = find_links(body)
        .into_iter()
        .find(|(link, _)| link.start < cut && link.end > cut)
        .map_or(cut, |(link, _)| link.end);
    (cut < body.len()).then_some(cut)
}

/// Text, its runs, and what clicking where does.
type TextLayout = (String, Vec<gpui_kit::TextRun>, Vec<(std::ops::Range<usize>, TextClick)>);

/// Message text laid out as runs: links underlined, @mentions as pills,
/// Signal's formatting (bold, italic, strikethrough, monospace), spoilers as
/// solid blocks until revealed, and room for the time at the end when
/// `spacer` is given (in the bubble's colour, `bg`, to draw it in). Returns
/// the text, its runs and what clicking where does.
fn text_runs(
    body: &str,
    mentions: &[signal_store::MentionSpan],
    styles: &[signal_store::StyleSpan],
    revealed: bool,
    fg: Hsla,
    spacer: Option<(&str, Hsla)>,
) -> TextLayout {
    use signal_store::TextStyle;
    let valid = |r: &std::ops::Range<usize>| r.end <= body.len() && body.is_char_boundary(r.start) && body.is_char_boundary(r.end);
    let links: Vec<(std::ops::Range<usize>, String)> = find_links(body);
    let mentions: Vec<std::ops::Range<usize>> =
        mentions.iter().map(|m| m.start..m.start + m.len).filter(|r| valid(r)).collect();
    let styles: Vec<(std::ops::Range<usize>, TextStyle)> =
        styles.iter().map(|s| (s.start..s.start + s.len, s.style)).filter(|(r, _)| valid(r)).collect();
    let mut cuts = vec![0, body.len()];
    cuts.extend(links.iter().flat_map(|(r, _)| [r.start, r.end]));
    cuts.extend(mentions.iter().flat_map(|r| [r.start, r.end]));
    cuts.extend(styles.iter().flat_map(|(r, _)| [r.start, r.end]));
    cuts.sort_unstable();
    cuts.dedup();
    let covers = |r: &std::ops::Range<usize>, at: usize| r.start <= at && at < r.end;
    let mut runs = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start == end {
            continue;
        }
        let has = |style: TextStyle| styles.iter().any(|(r, s)| *s == style && covers(r, start));
        let mention = mentions.iter().any(|r| covers(r, start));
        let hidden = has(TextStyle::Spoiler) && !revealed;
        let mut font = gpui_kit::font(if has(TextStyle::Monospace) { "Menlo" } else { ".SystemUIFont" });
        font.weight = if has(TextStyle::Bold) {
            FontWeight::BOLD
        } else if mention {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        if has(TextStyle::Italic) {
            font.style = gpui_kit::FontStyle::Italic;
        }
        runs.push(gpui_kit::TextRun {
            len: end - start,
            font,
            color: fg,
            // A hidden spoiler is a block of the text colour, as Desktop
            // covers it; a mention a soft pill in either bubble colour.
            background_color: if hidden {
                Some(fg)
            } else if mention {
                Some(gpui_kit::hsla(0., 0., 0.5, 0.22))
            } else {
                None
            },
            underline: links.iter().any(|(r, _)| covers(r, start)).then_some(gpui_kit::UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            }),
            strikethrough: has(TextStyle::Strikethrough).then_some(gpui_kit::StrikethroughStyle {
                thickness: px(1.),
                ..Default::default()
            }),
        });
    }
    let mut clicks: Vec<_> = links.into_iter().map(|(r, url)| (r, TextClick::Open(url))).collect();
    if !revealed {
        // A hidden spoiler takes the click, even over a link inside it.
        let spoilers: Vec<_> = styles.iter().filter(|(_, s)| *s == TextStyle::Spoiler).map(|(r, _)| r.clone()).collect();
        clicks.retain(|(r, _)| !spoilers.iter().any(|s| s.start < r.end && r.start < s.end));
        clicks.extend(spoilers.into_iter().map(|r| (r, TextClick::Reveal)));
    }
    let mut content = body.to_owned();
    if let Some((spacer, bg)) = spacer {
        push_run(&mut content, &mut runs, spacer, bg, FontWeight::NORMAL, false);
    }
    (content, runs, clicks)
}

/// `text` added to laid-out text in the system font.
fn push_run(content: &mut String, runs: &mut Vec<gpui_kit::TextRun>, text: &str, color: Hsla, weight: FontWeight, underline: bool) {
    let mut font = gpui_kit::font(".SystemUIFont");
    font.weight = weight;
    runs.push(gpui_kit::TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: underline.then_some(gpui_kit::UnderlineStyle { thickness: px(1.), ..Default::default() }),
        strikethrough: None,
    });
    content.push_str(text);
}

/// How a long message ends while only its start is here, in Desktop's words
/// (`ts/components/conversation/MessageBody.dom.tsx`): "..." and then what's
/// happening to the rest, which can be clicked to try again after a failed
/// download.
fn long_text_tail(state: signal_store::LongTextState) -> (&'static str, Option<TextClick>) {
    use signal_store::LongTextState;
    match state {
        LongTextState::Downloading => ("Downloading", None),
        LongTextState::Failed => ("Download Full Message", Some(TextClick::DownloadText)),
        LongTextState::TooBig => ("Message too long to display more", None),
    }
}

/// A message's text: see [`text_runs`]. Clicking a hidden spoiler shows the
/// message's spoilers. A long one shows `limit` characters (Desktop's
/// first 800 when `None`) and "Read more".
fn message_text(
    message: &Message,
    fg: Hsla,
    revealed: bool,
    limit: Option<usize>,
    spacer: Option<(&str, Hsla)>,
    cx: &mut Context<ConversationView>,
) -> AnyElement {
    let text = div().text_size(px(BODY_SIZE)).line_height(px(BODY_LINE));
    let limit = limit.unwrap_or(READ_MORE_INITIAL);
    let cut = read_more_cut(&message.body, limit);
    let (mut content, mut runs, mut clicks) = match cut {
        None => text_runs(&message.body, &message.mentions, &message.styles, revealed, fg, None),
        Some(cut) => {
            // Mentions and formatting stop at the cut too.
            let mentions: Vec<_> = message
                .mentions
                .iter()
                .filter(|m| m.start < cut)
                .map(|m| signal_store::MentionSpan { len: m.len.min(cut - m.start), ..m.clone() })
                .collect();
            let styles: Vec<_> = message
                .styles
                .iter()
                .filter(|s| s.start < cut)
                .map(|s| signal_store::StyleSpan { len: s.len.min(cut - s.start), ..*s })
                .collect();
            text_runs(&message.body[..cut], &mentions, &styles, revealed, fg, None)
        }
    };
    if cut.is_some() {
        // Desktop's `${text}...` and its " Read more" button.
        push_run(&mut content, &mut runs, "... ", fg, FontWeight::NORMAL, false);
        let start = content.len();
        push_run(&mut content, &mut runs, "Read more", fg, FontWeight::SEMIBOLD, true);
        clicks.push((start..content.len(), TextClick::ReadMore));
    } else if let Some(long_text) = &message.long_text {
        let (label, click) = long_text_tail(long_text.state);
        push_run(&mut content, &mut runs, "... ", fg, FontWeight::NORMAL, false);
        let start = content.len();
        push_run(&mut content, &mut runs, label, fg, FontWeight::SEMIBOLD, click.is_some());
        if let Some(click) = click {
            clicks.push((start..content.len(), click));
        }
    }
    if let Some((spacer, bg)) = spacer {
        push_run(&mut content, &mut runs, spacer, bg, FontWeight::NORMAL, false);
    }
    let styled = gpui_kit::StyledText::new(content).with_runs(runs);
    if clicks.is_empty() {
        return text.child(styled).into_any_element();
    }
    let (ranges, actions): (Vec<_>, Vec<_>) = clicks.into_iter().unzip();
    let message_id = message.id;
    let view = cx.entity().downgrade();
    text.child(gpui_kit::InteractiveText::new(("message-text", message_id as u64), styled).on_click(
        ranges,
        move |ix, _, cx| match &actions[ix] {
            TextClick::Open(url) => {
                if let Some(state) = view.upgrade().map(|v| v.read(cx).state.clone()) {
                    open_link(&state, url, cx);
                }
            }
            TextClick::Reveal => {
                let _ = view.update(cx, |this, cx| {
                    this.revealed_spoilers.insert(message_id);
                    cx.notify();
                });
            }
            TextClick::DownloadText => {
                let _ = view.update(cx, |this, cx| this.state.read(cx).send(Command::FetchLongText { message_id }));
            }
            TextClick::ReadMore => {
                let _ = view.update(cx, |this, cx| {
                    this.display_limits.insert(message_id, limit + READ_MORE_INCREMENT);
                    cx.notify();
                });
            }
        },
    ))
    .into_any_element()
}

/// A link clicked in a message or its preview: Signal's own links open
/// here (a group's join dialog, a call's lobby, a chat), as Desktop's
/// `handleUrl` routes them (app/main.main.ts); the rest go to the browser.
fn open_link(state: &Entity<AppState>, url: &str, cx: &mut App) {
    if !state.update(cx, |state, cx| state.open_signal_link(url, cx)) {
        cx.open_url(url);
    }
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

impl ConversationView {
    /// A reply's quote; clicking it shows the quoted message, as in Desktop.
    fn render_quote(&self, message_id: i64, quote: signal_store::Quote, fg: Hsla, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let (author, sent_at) = (quote.author.clone(), quote.sent_at);
        div()
            .id(("quote", message_id as u64))
            .debug_selector(move || format!("quote-{message_id}"))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                // Desktop's toast when it's gone (`icu:originalMessageNotFound`).
                this.go_to(author.clone(), sent_at, "Original message not found", cx);
            }))
            .child(render_quote(quote, fg, palette))
    }
}

fn render_quote(quote: signal_store::Quote, fg: Hsla, palette: Palette) -> impl IntoElement {
    // A column with a left border, not a row with a bar beside the text: in a
    // flex row the text is measured unwrapped and would widen the bubble past
    // its maximum. A thumbnail is placed over reserved padding for the same
    // reason.
    let thumbnail = quote.thumbnail.clone();
    // Audio has Desktop's microphone where a photo would go.
    let audio = thumbnail.is_none() && quote.attachment.as_ref().is_some_and(|a| a.content_type.starts_with("audio/"));
    let label = quote.attachment.as_ref().map(|a| a.label()).filter(|_| quote.text.trim().is_empty());
    div()
        .relative()
        .flex()
        .flex_col()
        .px_2()
        .py_1()
        .when(thumbnail.is_some() || audio, |d| d.pr(px(QUOTE_THUMBNAIL + 8.)).min_h(px(QUOTE_THUMBNAIL)))
        .when(audio, |d| {
            d.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(QUOTE_THUMBNAIL))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Mic).size_5().text_color(fg)),
            )
        })
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
        .map(|d| match label {
            Some(label) => d.child(div().text_sm().italic().truncate().child(label)),
            None => d.child(div().text_sm().line_clamp(3).text_ellipsis().child(collapse_whitespace(&quote.text))),
        })
}

/// A view-once bubble's icon, title and detail, in Desktop's words
/// (`renderTapToViewText`, `renderSimpleAttachmentNotAvailable`).
fn view_once_label(view_once: ViewOnce, outgoing: bool, first: Option<&AttachmentMeta>) -> (IconName, &'static str, Option<String>) {
    if outgoing {
        return (IconName::CircleDashed, "View Once Media", None);
    }
    match (view_once, first) {
        (ViewOnce::Viewed, _) => (IconName::CircleDashed, "Viewed", None),
        (ViewOnce::Ready, Some(a)) => {
            let video = a.content_type.starts_with("video/") || a.gif;
            let detail = match &a.local_path {
                Some(_) => "Click to view".to_owned(),
                None => crate::views::common::format_size(a.size),
            };
            (if video { IconName::Video } else { IconName::Image }, if video { "View Once Video" } else { "View Once Photo" }, Some(detail))
        }
        (ViewOnce::Gone, _) => (IconName::Ban, "View once media is not available", None),
        // Invalid, or ready with nothing to show.
        _ => (IconName::Ban, "Can't download view once media", None),
    }
}

/// What clicking a view-once message that can't open says
/// (`Message--tap-to-view--incoming--expired-toast`, `--outgoing--expired-toast-2`).
fn view_once_toast(view_once: ViewOnce, outgoing: bool) -> Option<&'static str> {
    match (view_once, outgoing) {
        (_, true) => Some("View-once media is deleted after sending"),
        (ViewOnce::Viewed, false) => Some("You already viewed this message."),
        _ => None,
    }
}

/// What the image viewer needs to show a downloaded image.
fn viewed_image(attachment: &AttachmentMeta) -> Option<ViewedImage> {
    Some(ViewedImage {
        path: std::path::PathBuf::from(attachment.local_path.as_ref()?),
        size: attachment.width.zip(attachment.height).filter(|(w, h)| *w > 0 && *h > 0),
        file_name: attachment.file_name.clone(),
        view_once: false,
        in_chat: None,
    })
}

/// Desktop's waveform: bar width, the gap between bars, and heights.
const WAVE_BAR: f32 = 2.;
const WAVE_GAP: f32 = 2.;
const WAVE_MIN: f32 = 4.;
const WAVE_MAX: f32 = 20.;

/// Desktop's audio player (`MessageAudio`): play or pause, the waveform
/// (played bars in full colour, click one to go there), the time left (or
/// the length), and while it plays the rate button.
fn render_audio(
    message_id: i64,
    index: usize,
    attachment: &AttachmentMeta,
    title: &str,
    fg: Hsla,
    state: &Entity<AppState>,
    cx: &mut Context<ConversationView>,
) -> AnyElement {
    let path = attachment.local_path.as_ref().map(std::path::PathBuf::from);
    let wave = path.as_ref().and_then(|p| state.update(cx, |state, cx| state.waveform(p, cx)));
    let (playing, position, duration, rate) = match state.read(cx).audio.as_ref() {
        Some(audio) if audio.message_id == message_id && audio.index == index => {
            let (position, duration) = audio.progress();
            (Some(audio.is_playing()), position, duration, audio.rate)
        }
        _ => (None, 0., wave.as_ref().map_or(0., |(_, seconds)| *seconds), 1.),
    };
    let played = if duration > 0. { (position / duration).clamp(0., 1.) } else { 0. };
    let peaks = wave.as_ref().map(|(peaks, _)| peaks.clone());
    let id = |what: &str| SharedString::from(format!("audio-{what}-{message_id}-{index}"));
    // The button: download first, then play or pause.
    let button = div()
        .id(id("button"))
        .size(px(36.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .bg(Hsla { a: 0.15, ..fg })
        .child(
            Icon::new(match (&path, playing) {
                (None, _) => IconName::Download,
                (Some(_), Some(true)) => IconName::Pause,
                _ => IconName::Play,
            })
            .size_4()
            .text_color(fg),
        )
        .on_click({
            let (state, path, title) = (state.clone(), path.clone(), title.to_owned());
            move |_, _, cx| match &path {
                Some(path) => state.update(cx, |state, cx| state.toggle_audio(message_id, index, path.clone(), title.clone(), cx)),
                None => state.read(cx).send(Command::FetchAttachment { message_id, index }),
            }
        });
    let bars = (0..crate::audio::BARS).map(|bar| {
        let peak = peaks.as_ref().and_then(|p| p.get(bar).copied()).unwrap_or(0.);
        // Not downloaded (or still decoding): flat, as Desktop shows it.
        let height = if peaks.is_some() { WAVE_MIN + peak * (WAVE_MAX - WAVE_MIN) } else { WAVE_BAR };
        let done = (bar as f64 + 0.5) / crate::audio::BARS as f64 <= played;
        div()
            .id(SharedString::from(format!("audio-bar-{message_id}-{index}-{bar}")))
            .h(px(WAVE_MAX))
            .w(px(WAVE_BAR + WAVE_GAP))
            .flex()
            .items_center()
            .when_some(path.clone(), |d, path| {
                let (state, title) = (state.clone(), title.to_owned());
                d.cursor_pointer().on_click(move |_, _, cx| {
                    let fraction = bar as f64 / crate::audio::BARS as f64;
                    state.update(cx, |state, cx| state.seek_audio(message_id, index, path.clone(), fraction, title.clone(), cx));
                })
            })
            .child(div().w(px(WAVE_BAR)).h(px(height)).rounded_full().bg(if done { fg } else { Hsla { a: 0.4, ..fg } }))
    });
    // Desktop counts down while it plays.
    let time = if playing.is_some() { crate::audio::clock(duration - position) } else { crate::audio::clock(duration) };
    div()
        .id(id("row"))
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .child(button)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().flex().items_center().children(bars))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(Hsla { a: 0.8, ..fg })
                        .child(if path.is_some() {
                            time
                        } else {
                            crate::views::common::format_size(attachment.size)
                        })
                        .when(playing.is_some(), |d| {
                            let state = state.clone();
                            d.child(
                                div()
                                    .id(id("rate"))
                                    .px_1p5()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .bg(Hsla { a: 0.15, ..fg })
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(crate::audio::rate_label(rate))
                                    .on_click(move |_, _, cx| state.update(cx, |state, cx| state.cycle_audio_rate(cx))),
                            )
                        }),
                ),
        )
        .into_any_element()
}

/// A video in a message: its first frame with a play button (Desktop's
/// look), which opens the player; before it's downloaded, a download button
/// and its size.
fn render_video(
    message_id: i64,
    index: usize,
    attachment: &AttachmentMeta,
    state: &Entity<AppState>,
    cx: &mut Context<ConversationView>,
) -> AnyElement {
    let (w, h) = media_size(attachment.width, attachment.height);
    let path = attachment.local_path.as_ref().map(std::path::PathBuf::from);
    // Its first frame once downloaded; before that, its backup thumbnail.
    let poster: Option<gpui_kit::ImageSource> = match &path {
        Some(p) => state.update(cx, |state, cx| state.poster(p, cx)).map(Into::into),
        None => attachment.thumbnail_path.as_ref().map(|t| std::path::PathBuf::from(t).into()),
    };
    let white = gpui_kit::hsla(0., 0., 1., 1.);
    let badge = |icon: IconName| {
        div()
            .size(px(48.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(gpui_kit::hsla(0., 0., 0., 0.55))
            .child(Icon::new(icon).size_6().text_color(white))
    };
    let file_name = attachment.file_name.clone();
    let size = attachment.size;
    div()
        .id(SharedString::from(format!("video-{message_id}-{index}")))
        .debug_selector(move || format!("video-{message_id}-{index}"))
        .relative()
        .w(px(w))
        .h(px(h))
        .rounded_lg()
        .overflow_hidden()
        .cursor_pointer()
        .bg(gpui_kit::hsla(0., 0., 0.1, 1.))
        .when_some(poster, |d, poster| d.child(img(poster).absolute().inset_0().size_full().object_fit(gpui_kit::ObjectFit::Cover)))
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(badge(if path.is_some() { IconName::Play } else { IconName::Download }))
                .when(path.is_none(), |d| {
                    d.child(div().text_xs().text_color(white).child(crate::views::common::format_size(size)))
                }),
        )
        .on_click(cx.listener(move |this, _, _, cx| match &path {
            Some(path) => {
                let in_chat = Some(crate::views::lightbox::InChat::new(this.id.clone(), message_id, index));
                let video = crate::views::video_viewer::ViewedVideo { path: path.clone(), file_name: file_name.clone(), view_once: false, in_chat };
                this.state.update(cx, |state, cx| state.view_video(video, cx));
            }
            None => this.state.read(cx).send(Command::FetchAttachment { message_id, index }),
        }))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_attachment(
    message_id: i64,
    index: usize,
    attachment: &AttachmentMeta,
    title: &str,
    fg: Hsla,
    palette: Palette,
    state: &Entity<AppState>,
    cx: &mut Context<ConversationView>,
) -> AnyElement {
    if attachment.content_type.starts_with("audio/") {
        return render_audio(message_id, index, attachment, title, fg, state, cx);
    }
    if attachment.content_type.starts_with("video/") && !attachment.gif && !attachment.sticker {
        return render_video(message_id, index, attachment, state, cx);
    }
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
                    let mut image = viewed.clone();
                    image.in_chat = Some(crate::views::lightbox::InChat::new(this.id.clone(), message_id, index));
                    this.state.update(cx, |state, cx| state.view_image(image, cx));
                }))
            })
            .child(img(std::path::PathBuf::from(path)).w(px(w)).h(px(h)).rounded_lg())
            .into_any_element();
    }
    // Not downloaded, but its backup thumbnail is: shown at the image's
    // size with the download button over it, as Desktop does.
    if let (true, Some(thumbnail)) = (is_image, attachment.thumbnail_path.as_ref()) {
        let (w, h) = media_size(attachment.width, attachment.height);
        let white = gpui_kit::hsla(0., 0., 1., 1.);
        return div()
            .id(SharedString::from(format!("attachment-{message_id}-{index}")))
            .debug_selector(move || format!("thumbnail-{message_id}-{index}"))
            .relative()
            .w(px(w))
            .h(px(h))
            .rounded_lg()
            .overflow_hidden()
            .cursor_pointer()
            .child(img(std::path::PathBuf::from(thumbnail)).absolute().inset_0().size_full().object_fit(gpui_kit::ObjectFit::Cover))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(48.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(gpui_kit::hsla(0., 0., 0., 0.55))
                            .child(Icon::new(IconName::Download).size_6().text_color(white)),
                    )
                    .child(div().text_xs().text_color(white).child(crate::views::common::format_size(attachment.size))),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.state.read(cx).send(Command::FetchAttachment { message_id, index })))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read fresh each draw; the focus subscriptions redraw it as focus
        // comes and goes.
        self.timeline_focused = self.timeline_focus.is_focused(window);
        let palette = Palette::get(cx);
        let entity = cx.entity();
        let typing = self.render_typing(palette, cx);
        // Messages that arrived while scrolled up wait below, even when the
        // list hasn't measured them yet (it can't tell where its end is then).
        let scrolled_up = self.has_newer
            || self.unseen > 0
            || (!self.list.is_following_tail() && !self.list.is_scrolled_to_end().unwrap_or(true));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .child(self.render_header(palette, cx))
            .children(self.render_spoofing_warning(palette, cx))
            .children(self.render_mini_player(palette, cx))
            .children(self.render_pins(palette, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .key_context(crate::shortcuts::TIMELINE)
                    .track_focus(&self.timeline_focus)
                    .on_action(cx.listener(|this, _: &timeline::SelectPrevious, window, cx| this.move_target(-1, window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::SelectNext, window, cx| this.move_target(1, window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::PreviousPage, window, cx| this.page_target(true, window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::NextPage, window, cx| this.page_target(false, window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::SelectFirst, window, cx| this.target_first(window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::SelectLast, window, cx| this.target_last(window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::OpenSelected, window, cx| this.open_targeted(window, cx)))
                    .on_action(cx.listener(|this, _: &timeline::LeaveTimeline, window, cx| this.leave_timeline(window, cx)))
                    .image_cache(self.image_cache.clone())
                    .when(self.messages.is_empty() && !self.has_more, |d| d.children(self.render_hero(cx)))
                    .child(
                        list(self.list.clone(), move |ix, _window, cx| {
                            entity.update(cx, |this, cx| {
                                let palette = Palette::get(cx);
                                this.render_item(ix, palette, cx)
                            })
                        })
                        .size_full(),
                    )
                    .child(
                        gpui_kit::component::scroll::Scrollbar::vertical(&self.list)
                            .mode(gpui_kit::component::scroll::ScrollbarMode::Hover),
                    )
                    .children(self.render_floating_date(palette))
                    .when(scrolled_up, |d| d.child(self.render_scroll_down_buttons(palette, cx)))
                    .when(self.reaction_picker_for.is_some(), |d| {
                        d.child(deferred(
                            div()
                                .absolute()
                                .bottom_2()
                                .right_4()
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.reaction_picker_for = None;
                                    cx.notify();
                                }))
                                .child(self.picker.clone()),
                        ))
                    }),
            )
            .when_some(typing, |d, t| d.child(t))
            .map(|d| {
                if let Some(selected) = &self.selecting {
                    return d.child(self.render_select_bar(selected, palette, cx));
                }
                // Release notes are read-only: no composer, as Desktop's.
                if self.id.is_signal() {
                    return d;
                }
                // A request to join, or announcement-only (a message request,
                // an invitation among them, has the bar below, as Desktop's
                // `CompositionArea` puts it first).
                if let Some(chat) = self.state.read(cx).conversation(&self.id)
                    && !crate::views::message_request::shows_bar(chat)
                    && let Some(replacement) = crate::views::group_admin::composer_replacement(&self.state, chat, palette, cx)
                {
                    return d.child(replacement);
                }
                let state = self.state.read(cx);
                let chat = state.conversation(&self.id);
                match chat.filter(|c| c.blocked || c.left || c.ended || !c.accepted) {
                    // Desktop's `CompositionArea--group-terminated`.
                    Some(conversation) if conversation.ended => d.child(
                        div()
                            .id("group-ended")
                            .debug_selector(|| "group-ended".into())
                            .flex_none()
                            .px_6()
                            .py_4()
                            .border_t_1()
                            .border_color(palette.panel_border)
                            .text_sm()
                            .text_center()
                            .text_color(palette.text_secondary)
                            .child("You can't send messages because the group has ended."),
                    ),
                    // Desktop's message request bar (blocked chats too).
                    Some(conversation) if crate::views::message_request::shows_bar(conversation) => {
                        let shared = conversation
                            .id
                            .as_direct()
                            .map_or(0, |aci| crate::views::message_request::shared_groups(state, aci).len());
                        let bar = crate::views::message_request::render_bar(
                            conversation,
                            &self.state,
                            self.added_by_name(cx),
                            shared,
                            palette,
                            cx,
                        );
                        d.child(bar)
                    }
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

/// Desktop's colors for the Signal chat (`--axo-color-legacy-signal-*` and
/// `-official-chat-badge-*` in `ts/axo/_tailwind-theme/colors.css`).
struct SignalColors {
    background: Hsla,
    message: Hsla,
    badge_bg: Hsla,
    badge_text: Hsla,
}

fn signal_colors(cx: &App) -> SignalColors {
    let dark = gpui_kit::component::ActiveTheme::theme(cx).is_dark();
    let rgb = |hex: u32| Hsla::from(gpui_kit::rgb(hex));
    let alpha = |hex: u32, a: f32| Hsla { a, ..rgb(hex) };
    if dark {
        SignalColors { background: rgb(0x2f3240), message: rgb(0x444664), badge_bg: alpha(0x4952f8, 0.4), badge_text: rgb(0xc2c5fe) }
    } else {
        SignalColors { background: rgb(0xf6f7ff), message: rgb(0x8889b4), badge_bg: alpha(0x4655ff, 0.12), badge_text: rgb(0x030ffc) }
    }
}

/// Desktop's hero at the top of the Signal chat (ConversationHero): its
/// avatar and name, the "Official chat" pill, and what the chat is.
fn signal_hero(palette: Palette, cx: &App) -> impl IntoElement {
    let colors = signal_colors(cx);
    div().debug_selector(|| "signal-hero".into()).pt_6().pb_2().px_6().flex().justify_center().child(
        div()
            .max_w(px(360.))
            .px_6()
            .py_5()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .rounded_xl()
            .border_1()
            .border_color(palette.panel_border)
            .bg(colors.background)
            .child(crate::views::common::signal_avatar(px(80.)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Signal")
                    .child(crate::views::common::official_badge(palette)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2p5()
                    .py_1()
                    .rounded_full()
                    .bg(colors.badge_bg)
                    .text_color(colors.badge_text)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(Icon::new(IconName::BadgeCheck).size_4())
                    .child("Official chat"),
            )
            .child(
                div()
                    .text_sm()
                    .text_center()
                    .child("The only official chat from Signal. Keep up to date with news and release notes."),
            ),
    )
}

/// Where a release note's "Donate" goes.
const DONATE_URL: &str = "https://signal.org/donate/";

/// The local day a time falls on, as a number (Desktop's `getMidnight`).
fn day_number(millis: i64) -> i64 {
    use chrono::{Datelike as _, TimeZone as _};
    chrono::Local.timestamp_millis_opt(millis).single().map_or(0, |t| i64::from(t.date_naive().num_days_from_ce()))
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
    let (state, note_to_self, chat, ended) =
        this.read_with(cx, |this, cx| (this.state.clone(), this.note_to_self(cx), this.delete_chat(cx), this.ended(cx)));
    let everyone = everyone_delete(std::slice::from_ref(message), chat, now_millis());
    let failed = message.direction == Direction::Outgoing
        && matches!(message.status, DeliveryStatus::Failed | DeliveryStatus::PartialSent)
        && !message.deleted;
    let retry_delete = this.read_with(cx, |this, cx| this.can_retry_delete(message, cx));
    let with_view = |f: fn(&mut ConversationView, i64, &mut Window, &mut Context<ConversationView>)| {
        let view = view.clone();
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = view.update(cx, |this, cx| f(this, id, window, cx));
        }
    };
    let mut menu = menu;
    // Desktop doesn't forward polls or view-once messages (`canForward`).
    if !message.deleted && message.poll.is_none() && message.view_once.is_none() {
        let state = state.clone();
        menu = menu.item(
            PopupMenuItem::new("Forward")
                .icon(IconName::Forward)
                .on_click(move |_, window, cx| crate::views::forward::open(state.clone(), vec![id], window, cx)),
        );
    }
    if !ended && signal_backend::edits::can_edit(message, note_to_self, now_millis()) {
        let editing = message.clone();
        let view = view.clone();
        menu = menu.item(PopupMenuItem::new("Edit").icon(IconName::Pencil).on_click(move |_, window, cx| {
            let editing = editing.clone();
            let _ = view.update(cx, |this, cx| this.composer.update(cx, |composer, cx| composer.start_edit(editing, window, cx)));
        }));
    }
    let (pinned, can_pin, pin_count) = this.read_with(cx, |this, cx| {
        // Desktop's `canPinMessages`: not in an ended group.
        let can = !this.id.is_signal() && this.state.read(cx).conversation(&this.id).is_some_and(|c| c.can_edit_info && !c.ended);
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
    // Desktop's "End poll", on our own poll until it's ended.
    if !ended && message.direction == Direction::Outgoing && message.poll.as_ref().is_some_and(|p| !p.ended) && !message.deleted {
        let state = state.clone();
        menu = menu.item(
            PopupMenuItem::new("End poll")
                .icon(IconName::ChartBar)
                .on_click(move |_, _, cx| state.read(cx).send(Command::EndPoll { message_id: id })),
        );
    }
    // A poll has no text to copy (its body is the question).
    if !message.body.is_empty() && !message.deleted && message.poll.is_none() {
        let body = message.body.clone();
        menu = menu.item(
            PopupMenuItem::new("Copy text")
                .icon(IconName::Copy)
                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(body.clone()))),
        );
    }
    let (details, info_state) = (message.clone(), state.clone());
    menu = menu.item(
        PopupMenuItem::new("Info")
            .icon(IconName::Info)
            .on_click(move |_, window, cx| crate::views::message_info::open(info_state.clone(), details.clone(), window, cx)),
    );
    let delete_state = state.clone();
    menu = menu.item(PopupMenuItem::new("Delete").icon(IconName::Trash).on_click(move |_, window, cx| {
        confirm_delete(delete_state.clone(), vec![id], note_to_self, everyone, window, cx)
    }));
    if failed {
        let state = state.clone();
        menu = menu.item(
            PopupMenuItem::new("Retry Send")
                .icon(IconName::RotateCcw)
                .on_click(move |_, _, cx| state.read(cx).send(Command::RetrySend { message_id: id })),
        );
    }
    // Desktop's `MessageContextMenu`: after "Retry Send".
    if retry_delete {
        menu = menu.item(
            PopupMenuItem::new("Retry Delete for Everyone")
                .icon(IconName::Trash)
                .on_click(move |_, _, cx| state.read(cx).send(Command::RetryDeleteForEveryone { message_id: id })),
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
                        if pinned < signal_backend::remote_config().pinned_message_limit() {
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

/// An attachment over Signal's receive limit, which isn't downloaded or
/// shown (Desktop's `wasTooBig`, `ts/jobs/AttachmentDownloadManager.preload.ts`).
fn too_big(attachment: &AttachmentMeta) -> bool {
    attachment.local_path.is_none() && attachment.size > signal_backend::remote_config().attachment_max_receive_bytes()
}

/// What stands in for attachments too big to receive, in Desktop's words
/// (`renderAttachmentTooBig` in `ts/components/conversation/Message.dom.tsx`).
fn too_big_text(message: &Message) -> Option<&'static str> {
    if !message.attachments.iter().any(too_big) {
        None
    } else if message.attachments.iter().all(too_big) {
        Some("Attachment too large to display.")
    } else {
        Some("Some attachments are too large to display.")
    }
}

fn too_big_label(message: &Message) -> Option<AnyElement> {
    let label = too_big_text(message)?;
    let id = message.id;
    Some(div().debug_selector(move || format!("too-big-{id}")).italic().child(label).into_any_element())
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

/// Whether messages can be deleted for everyone, and whether Desktop's
/// admin delete confirmation goes first (`getMessagesCanDeleteForEveryone`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EveryoneDelete {
    allowed: bool,
    /// Some need a group admin's delete, and not all are ours.
    confirm_admin: bool,
}

fn everyone_delete(messages: &[Message], chat: signal_backend::deletes::Chat, now: i64) -> EveryoneDelete {
    use signal_backend::deletes::{Kind, MAX_FOR_EVERYONE, send_kind};
    let kinds: Option<Vec<Kind>> = messages.iter().map(|m| send_kind(m, chat, now)).collect();
    let Some(kinds) = kinds.filter(|k| !k.is_empty() && k.len() <= MAX_FOR_EVERYONE) else {
        return EveryoneDelete::default();
    };
    let own = messages.iter().all(|m| m.direction == Direction::Outgoing);
    EveryoneDelete { allowed: true, confirm_admin: !own && kinds.contains(&Kind::Admin) }
}

/// Desktop's delete dialog: for you (on all your devices), or for everyone
/// while that's still allowed. Note to Self only has the first.
fn confirm_delete(
    state: Entity<AppState>,
    message_ids: Vec<i64>,
    note_to_self: bool,
    everyone: EveryoneDelete,
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
        let (mine, for_all) = (state.clone(), state.clone());
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
                    .when(everyone.allowed && !note_to_self, |footer| {
                        footer.child(
                            Button::new("delete-for-everyone")
                                .label("Delete for everyone")
                                .danger()
                                .on_click(move |_, window, cx| {
                                    window.close_all_dialogs(cx);
                                    if everyone.confirm_admin && !for_all.read(cx).seen_admin_delete_education {
                                        confirm_admin_delete(for_all.clone(), all_ids.clone(), window, cx);
                                    } else {
                                        delete_for_everyone(&for_all, &all_ids, cx);
                                    }
                                }),
                        )
                    }),
            )
    });
}

fn delete_for_everyone(state: &Entity<AppState>, message_ids: &[i64], cx: &mut App) {
    for message_id in message_ids {
        state.read(cx).send(Command::DeleteForEveryone { message_id: *message_id });
    }
}

/// Desktop's admin delete confirmation, shown until it's been confirmed
/// once (`DeleteMessagesConfirmAdminDeleteDialog`, with the rollout
/// wording Desktop uses now). Cancel goes back to the first dialog.
fn confirm_admin_delete(state: Entity<AppState>, message_ids: Vec<i64>, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    let these = if message_ids.len() == 1 { "this message" } else { "these messages" };
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let (back, confirm) = (state.clone(), state.clone());
        let (back_ids, ids) = (message_ids.clone(), message_ids.clone());
        dialog
            .title("Delete for everyone?")
            .w(px(420.))
            .child(div().text_sm().text_color(palette.text_secondary).child(format!(
                "Group members on the latest versions of Signal will see that you deleted {these}."
            )))
            .footer(
                DialogFooter::new()
                    .child(Button::new("admin-delete-cancel").label("Cancel").outline().on_click(move |_, window, cx| {
                        window.close_all_dialogs(cx);
                        let everyone = EveryoneDelete { allowed: true, confirm_admin: true };
                        confirm_delete(back.clone(), back_ids.clone(), false, everyone, window, cx);
                    }))
                    .child(Button::new("admin-delete-confirm").label("Delete for everyone").danger().on_click(
                        move |_, window, cx| {
                            confirm.update(cx, |state, cx| state.set_seen_admin_delete_education(cx));
                            delete_for_everyone(&confirm, &ids, cx);
                            window.close_all_dialogs(cx);
                        },
                    )),
            )
    });
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}


/// The timeline's keyboard, as Signal Desktop's (`Timeline.dom.tsx`
/// `#handleKeyDown`, `Message.dom.tsx` `#handleOpen`, and the message
/// shortcuts in `addGlobalKeyboardShortcuts.preload.ts`): a targeted
/// message that the arrow keys move and the message shortcuts act on.
impl ConversationView {
    /// The targeted message's place in the loaded list.
    fn targeted_index(&self) -> Option<usize> {
        let id = self.targeted?;
        self.messages.iter().position(|m| m.id == id)
    }

    fn targeted_message(&self) -> Option<Message> {
        self.targeted_index().map(|ix| self.messages[ix].clone())
    }

    /// Targets the item at `ix`, shows it, and gives the timeline the
    /// keyboard.
    fn target(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(message) = self.messages.get(ix) else { return };
        self.targeted = Some(message.id);
        self.list.scroll_to_reveal_item(ix);
        window.focus(&self.timeline_focus, cx);
        cx.notify();
    }

    /// ↑ and ↓: the item above or below, stopping at the ends (older
    /// messages load as the list nears the top).
    fn move_target(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.targeted_index() else { return };
        let next = ix as isize + step;
        if next >= 0 && (next as usize) < self.messages.len() {
            self.target(next as usize, window, cx);
        }
    }

    /// Page Up and Page Down: the item about a screen away.
    fn page_target(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut ix) = self.targeted_index() else { return };
        let page = self.list.viewport_bounds().size.height;
        let mut travelled = px(0.);
        loop {
            let next = if up { ix.checked_sub(1) } else { Some(ix + 1).filter(|&n| n < self.messages.len()) };
            let Some(next) = next else { break };
            ix = next;
            // Items not laid out yet count as a typical message.
            travelled += self.list.bounds_for_item(ix).map_or(px(60.), |b| b.size.height);
            if travelled >= page {
                break;
            }
        }
        self.target(ix, window, cx);
    }

    /// Home or ⌘↑: the first item loaded.
    fn target_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.targeted.is_some() && !self.messages.is_empty() {
            self.target(0, window, cx);
        }
    }

    /// End or ⌘↓: down to the newest message (Desktop's `#scrollDown`),
    /// targeted once it's loaded.
    fn target_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_newer {
            self.target_newest = true;
            self.load_initial(cx);
            self.caught_up(cx);
            window.focus(&self.timeline_focus, cx);
            return;
        }
        self.list.set_follow_mode(FollowMode::Tail);
        if let Some(last) = self.messages.len().checked_sub(1) {
            self.target(last, window, cx);
        }
        self.list.scroll_to_end();
    }

    /// Desktop's ⌘J: the oldest message that was unread when the chat
    /// opened (where its "unread messages" line is), or the last one.
    pub fn target_oldest_unread_or_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unread = self.unread_at_open as usize;
        let incoming: Vec<usize> = (0..self.messages.len())
            .filter(|&ix| self.messages[ix].notice.is_none() && self.messages[ix].direction == Direction::Incoming)
            .collect();
        let oldest_unread = (unread > 0).then(|| incoming.len().checked_sub(unread).map_or(incoming.first(), |at| incoming.get(at))).flatten();
        match oldest_unread.copied().or(self.messages.len().checked_sub(1)) {
            Some(ix) => self.target(ix, window, cx),
            None => window.focus(&self.timeline_focus, cx),
        }
    }

    /// Esc in the timeline: back to typing.
    fn leave_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.targeted = None;
        let handle = self.composer.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Enter or Space on the targeted message: Desktop's `#handleOpen`. A
    /// view-once message opens (or says why it can't); a file opens in its
    /// app; a voice message or audio plays; anything not downloaded starts
    /// downloading; a photo or video opens full-window.
    fn open_targeted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(message) = self.targeted_message() else { return };
        if message.deleted || message.notice.is_some() {
            return;
        }
        let id = message.id;
        let outgoing = message.direction == Direction::Outgoing;
        if let Some(view_once) = message.view_once {
            let first = message.attachments.first();
            match (view_once, outgoing, first) {
                (ViewOnce::Ready, false, Some(a)) if a.local_path.is_some() => {
                    self.state.read(cx).send(Command::OpenViewOnce { message_id: id })
                }
                (ViewOnce::Ready, false, Some(_)) => self.state.read(cx).send(Command::FetchAttachment { message_id: id, index: 0 }),
                _ => {
                    if let Some(toast) = view_once_toast(view_once, outgoing) {
                        use gpui_kit::component::WindowExt as _;
                        window.push_notification(gpui_kit::component::notification::Notification::new().message(toast), cx);
                    }
                }
            }
            return;
        }
        let Some(first) = message.attachments.first() else { return };
        if too_big(first) {
            return;
        }
        let kind = &first.content_type;
        let media = (kind.starts_with("image/") || kind.starts_with("video/")) && !first.sticker;
        let audio = kind.starts_with("audio/");
        match first.local_path.as_ref().map(std::path::PathBuf::from) {
            // Desktop's generic attachment: opened.
            Some(path) if !media && !audio && !first.sticker => cx.open_with_system(&path),
            Some(path) if audio => {
                let title = self.reactor_name(message.author.as_deref(), cx);
                self.state.update(cx, |state, cx| state.toggle_audio(id, 0, path, title, cx));
            }
            None if !first.sticker => self.state.read(cx).send(Command::FetchAttachment { message_id: id, index: 0 }),
            // In the lightbox, to step through the chat's media from.
            Some(path) if kind.starts_with("video/") && !first.gif => {
                let in_chat = Some(crate::views::lightbox::InChat::new(self.id.clone(), id, 0));
                self.state.update(cx, |state, cx| {
                    let video = crate::views::video_viewer::ViewedVideo { path, file_name: first.file_name.clone(), view_once: false, in_chat };
                    state.view_video(video, cx);
                })
            }
            Some(_) if kind.starts_with("image/") => {
                if let Some(mut image) = viewed_image(first) {
                    image.in_chat = Some(crate::views::lightbox::InChat::new(self.id.clone(), id, 0));
                    self.state.update(cx, |state, cx| state.view_image(image, cx));
                }
            }
            _ => {}
        }
    }

    /// Desktop's ⌘D: the targeted message's details.
    pub fn show_targeted_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(message) = self.targeted_message().filter(|m| m.notice.is_none()) {
            crate::views::message_info::open(self.state.clone(), message, window, cx);
        }
    }

    /// Whether replies and reactions are offered here (Desktop's
    /// `canReplyOrReact`): not in an ended group, a blocked chat or a
    /// message request, nor the Signal chat.
    fn can_reply_or_react(&self, cx: &App) -> bool {
        !self.ended(cx)
            && !self.id.is_signal()
            && !self.state.read(cx).conversation(&self.id).is_some_and(|c| c.blocked || !c.accepted)
    }

    /// Desktop's ⌘⇧R: a reply being written is dropped; otherwise the
    /// targeted message is replied to.
    pub fn toggle_reply_to_targeted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self
            .targeted_message()
            .filter(|m| m.notice.is_none() && !m.deleted && self.can_reply_or_react(cx));
        self.composer.update(cx, |composer, cx| composer.toggle_reply(message, window, cx));
    }

    /// Desktop's ⌘⇧E (`TimelineMessage`'s `onReactShortcut`): the targeted
    /// message's reactions open or close.
    pub fn toggle_reactions_for_targeted(&mut self, cx: &mut Context<Self>) {
        let Some(message) = self.targeted_message() else { return };
        if message.notice.is_some() || message.deleted || !self.can_reply_or_react(cx) {
            return;
        }
        self.reaction_bar_for = if self.reaction_bar_for == Some(message.id) { None } else { Some(message.id) };
        cx.notify();
    }

    /// Desktop's ⌘S: the targeted message's attachments, saved.
    pub fn save_targeted(&mut self, cx: &mut Context<Self>) {
        let Some(message) = self.targeted_message() else { return };
        // View-once media can't be saved (Desktop's `canDownload`).
        let downloadable: Vec<AttachmentMeta> = message
            .attachments
            .iter()
            .filter(|a| a.local_path.is_some() && !a.sticker && message.view_once.is_none())
            .cloned()
            .collect();
        if !downloadable.is_empty() {
            download_attachments(downloadable, cx);
        }
    }

    /// What ⌘⇧D and ⌘⇧S act on: the messages picked in select mode, or
    /// the targeted one (Desktop's `getSelectedOrTargetedMessageIds`).
    fn selected_or_targeted(&self) -> Vec<Message> {
        match &self.selecting {
            Some(selected) if !selected.is_empty() => self.messages.iter().filter(|m| selected.contains(&m.id)).cloned().collect(),
            _ => self.targeted_message().filter(|m| m.notice.is_none()).into_iter().collect(),
        }
    }

    /// Desktop's ⌘⇧D: the delete dialog for them.
    pub fn delete_selected_or_targeted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        let chosen = self.selected_or_targeted();
        if chosen.is_empty() || window.has_active_dialog(cx) {
            return;
        }
        let everyone = everyone_delete(&chosen, self.delete_chat(cx), now_millis());
        let ids = chosen.iter().map(|m| m.id).collect();
        confirm_delete(self.state.clone(), ids, self.note_to_self(cx), everyone, window, cx);
        self.selecting = None;
        cx.notify();
    }

    /// Desktop's ⌘⇧S: the forward dialog for them, when they can go
    /// (not deleted, polls or view-once, and not too many).
    pub fn forward_selected_or_targeted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        let chosen = self.selected_or_targeted();
        let forwardable = !chosen.is_empty()
            && chosen.len() <= crate::views::forward::MAX_MESSAGES
            && chosen.iter().all(|m| !m.deleted && m.poll.is_none() && m.view_once.is_none());
        if !forwardable || window.has_active_dialog(cx) {
            return;
        }
        self.selecting = None;
        crate::views::forward::open(self.state.clone(), chosen.iter().map(|m| m.id).collect(), window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    fn message(id: i64, at: i64) -> Message {
        Message {
            id,
            conversation_id: signal_store::ConversationId("d:a".into()),
            sent_at: at,
            received_at: at,
            direction: Direction::Incoming,
            author: Some("a".into()),
            author_name: None,
            body: "hi".into(),
            status: DeliveryStatus::Delivered,
            reactions: Vec::new(),
            quote: None,
            attachments: Vec::new(),
            link_preview: None,
            deleted: false,
            edited: false,
            expires_in: 0,
            expire_start: None,
            edit_count: 0,
            notice: None,
            mentions: Vec::new(),
            styles: Vec::new(),
            cta: None,
            deleted_by_admin: None,
            long_text: None,
            poll: None,
            view_once: None,
            content: None,
            sealed_sender: Vec::new(),
            is_read: true,
        }
    }

    #[test]
    fn reactions_end_a_group_as_in_desktop() {
        let (older, newer) = (message(1, 1_000), message(2, 2_000));
        assert!(super::same_group(&older, &newer));
        let mut reacted = older.clone();
        reacted.reactions.push(signal_store::Reaction { emoji: "😂".into(), author: None, sent_at: 1_500 });
        assert!(!super::same_group(&reacted, &newer), "the reacted message ends its group");
    }

    /// As a group admin, deleting theirs asks first; deleting only ours
    /// doesn't (Desktop's `isDeletingOwnMessages`).
    #[test]
    fn admin_deletes_confirm_unless_all_are_ours() {
        use signal_backend::deletes::Chat;
        let now = 10_000;
        let admin = Chat { note_to_self: false, group: true, we_are_admin: true, admin_deletes: true, ended: false };
        let theirs = message(3, now - 100);
        let ours = Message { direction: Direction::Outgoing, author: None, ..message(4, now - 100) };
        let everyone = |m: &[Message], chat| super::everyone_delete(m, chat, now);
        let confirm = super::EveryoneDelete { allowed: true, confirm_admin: true };
        assert_eq!(everyone(std::slice::from_ref(&theirs), admin), confirm);
        assert_eq!(everyone(&[theirs.clone(), ours.clone()], admin), confirm);
        assert_eq!(everyone(std::slice::from_ref(&ours), admin), super::EveryoneDelete { allowed: true, confirm_admin: false });
        assert_eq!(everyone(std::slice::from_ref(&theirs), Chat { we_are_admin: false, ..admin }), super::EveryoneDelete::default());
        assert_eq!(everyone(&[], admin), super::EveryoneDelete::default());
        assert_eq!(everyone(&vec![theirs; 31], admin), super::EveryoneDelete::default(), "Desktop's limit of 30");
    }

    #[test]
    fn formatting_becomes_runs_and_spoilers_hide_until_shown() {
        use signal_store::{StyleSpan, TextStyle};
        let body = "bold it see https://x.org end";
        let at = |text: &str| body.find(text).unwrap();
        let styles = [
            StyleSpan { start: 0, len: 4, style: TextStyle::Bold },
            StyleSpan { start: at("it"), len: 2, style: TextStyle::Italic },
            // A spoiler with a link in it.
            StyleSpan { start: at("see"), len: "see https://x.org".len(), style: TextStyle::Spoiler },
        ];
        let fg = gpui_kit::hsla(0., 0., 0., 1.);
        let (content, runs, clicks) = super::text_runs(body, &[], &styles, false, fg, None);
        assert_eq!(content, body);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), body.len(), "runs cover the text");
        let run_at = |offset: usize| {
            let mut start = 0;
            runs.iter()
                .find(|r| {
                    let found = (start..start + r.len).contains(&offset);
                    start += r.len;
                    found
                })
                .unwrap()
        };
        assert_eq!(run_at(0).font.weight, FontWeight::BOLD);
        assert_eq!(run_at(at("it")).font.style, gpui_kit::FontStyle::Italic);
        assert_eq!(run_at(at("see")).background_color, Some(fg), "hidden: a block");
        // Hidden, the spoiler takes the click (its link waits until shown).
        assert_eq!(clicks, [(at("see")..at(" end"), super::TextClick::Reveal)]);
        let (_, runs, clicks) = super::text_runs(body, &[], &styles, true, fg, None);
        assert!(runs.iter().all(|r| r.background_color.is_none()));
        assert_eq!(clicks, [(at("https")..at(" end"), super::TextClick::Open("https://x.org".into()))]);
    }

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

    #[test]
    fn read_more_cuts_where_desktops_does() {
        // Up to 100 past the limit shows whole.
        assert_eq!(read_more_cut(&"a".repeat(900), 800), None);
        assert_eq!(read_more_cut(&"a".repeat(901), 800), Some(800));
        // Characters are UTF-16 units: "é" is one, "😀" two, both whole.
        assert_eq!(read_more_cut(&"é".repeat(901), 800), Some(1600), "800 two-byte characters");
        let emoji = "😀".repeat(451);
        assert_eq!(read_more_cut(&emoji, 800), Some(400 * 4));
        let odd = format!("a{}", "😀".repeat(451));
        assert_eq!(read_more_cut(&odd, 800), Some(1 + 400 * 4), "the emoji starting at unit 799 still shows");
        // A link the cut would split shows whole.
        let link = format!("{} https://example.com/a-long-path {}", "a".repeat(789), "b".repeat(200));
        assert_eq!(read_more_cut(&link, 800), Some(790 + "https://example.com/a-long-path".len()));
        // Each "Read more" adds 3,000.
        let long = "a".repeat(10_000);
        assert_eq!(read_more_cut(&long, READ_MORE_INITIAL + READ_MORE_INCREMENT), Some(3800));
        assert_eq!(read_more_cut(&long, 9_900), None);
    }

    #[gpui_kit::test]
    fn read_more_shows_a_long_message_bit_by_bit(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, NOTE_TO_SELF, 1, |_| true);
        let db = backend.db().clone();
        let sent_at = chrono::Utc::now().timestamp_millis();
        let body = "All the way up the ridge and back. ".repeat(60);
        let id = futures::executor::block_on(backend.spawn(async move {
            db.insert_message(&signal_store::NewMessage {
                conversation_id: ConversationId(NOTE_TO_SELF.into()),
                sent_at,
                received_at: sent_at,
                direction: Direction::Outgoing,
                author: None,
                author_name: None,
                body,
                status: DeliveryStatus::Sent,
                quote: None,
                attachments: vec![],
                link_preview: None,
                expires_in: 0,
            })
            .await
        }))
        .unwrap();
        let (mut cx, view) = open_view(cx, &backend, NOTE_TO_SELF);
        let short = bounds(&mut cx, format!("bubble-{id}"));
        cx.update(|_, cx| {
            view.update(cx, |this, cx| {
                this.display_limits.insert(id, READ_MORE_INITIAL + READ_MORE_INCREMENT);
                cx.notify();
            })
        });
        let full = bounds(&mut cx, format!("bubble-{id}"));
        assert!(full.size.height > short.size.height * 1.5, "{short:?} then {full:?}");
    }

    #[gpui_kit::test]
    fn shared_contacts_payments_and_gift_badges_show_as_desktops_do(cx: &mut TestAppContext) {
        const LEO: &str = "d:00000000-0000-4000-8000-0000000000f1";
        const SAM: &str = "d:00000000-0000-4000-8000-0000000000e0";
        const PRIYA: &str = "d:00000000-0000-4000-8000-0000000000c0";
        let (backend, _dir) = demo_backend(cx);
        let contact = wait_for_messages(&backend, LEO, 1, |m| m.content.is_some()).remove(0);
        let payment = wait_for_messages(&backend, SAM, 1, |m| m.content.is_some()).remove(0);
        let gift = wait_for_messages(&backend, PRIYA, 1, |m| m.content.is_some()).remove(0);

        // The payment notification: who, the phone has its status, the note.
        let (mut sam, _view) = open_view(cx, &backend, SAM);
        bounds(&mut sam, format!("bubble-{}", payment.id));
        // Our gift for Priya, opened on the phone: "View" opens Desktop's
        // thank-you dialog.
        let (mut priya, _view) = open_view(cx, &backend, PRIYA);
        bounds(&mut priya, format!("bubble-{}", gift.id));
        let view: &'static str = Box::leak(format!("gift-badge-{}", gift.id).into_boxed_str());
        crate::views::test_support::press(&mut priya, view);
        assert!(crate::views::test_support::shows(&mut priya, "outgoing-gift-badge"));

        // The contact: its card opens Desktop's details. Rosa has no chat
        // here, so there's no "Message".
        let (mut leo, _view) = open_view(cx, &backend, LEO);
        bounds(&mut leo, format!("bubble-{}", contact.id));
        let card: &'static str = Box::leak(format!("shared-contact-{}", contact.id).into_boxed_str());
        leo.update(|window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(format!("shared-contact-message-{}", contact.id)).is_none());
        });
        crate::views::test_support::press(&mut leo, card);
        assert!(crate::views::test_support::shows(&mut leo, "shared-contact-detail"));
    }

    #[test]
    fn a_long_message_ends_as_desktops_does_until_its_text_comes() {
        use signal_store::LongTextState;
        assert_eq!(long_text_tail(LongTextState::Downloading), ("Downloading", None));
        assert_eq!(long_text_tail(LongTextState::Failed), ("Download Full Message", Some(TextClick::DownloadText)));
        assert_eq!(long_text_tail(LongTextState::TooBig), ("Message too long to display more", None));
    }

    #[gpui_kit::test]
    fn a_long_message_shows_its_start_until_its_text_comes(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, NOTE_TO_SELF, 1, |_| true);
        let db = backend.db().clone();
        let sent_at = chrono::Utc::now().timestamp_millis();
        let id = futures::executor::block_on(backend.spawn(async move {
            let id = db
                .insert_message(&signal_store::NewMessage {
                    conversation_id: ConversationId(NOTE_TO_SELF.into()),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body: "The start of a long story".into(),
                    status: DeliveryStatus::Sent,
                    quote: None,
                    attachments: vec![],
                    link_preview: None,
                    expires_in: 0,
                })
                .await?;
            let long_text = signal_store::LongText {
                remote: signal_store::RemoteAttachment {
                    cdn: 3,
                    cdn_key: Some("made-up".into()),
                    key: "00".repeat(64),
                    digest: Some("00".repeat(32)),
                    plaintext_hash: None,
                    media_tier_cdn: None,
                },
                size: 4000,
                mentions: vec![],
                styles: vec![],
                state: signal_store::LongTextState::Failed,
                attempts: 0,
                retry_at: None,
            };
            db.set_long_text(id, Some(&long_text)).await?;
            Ok::<_, signal_store::Error>(id)
        }))
        .unwrap();
        let mut cx = open(cx, &backend, NOTE_TO_SELF);
        let bubble = bounds(&mut cx, format!("bubble-{id}"));
        assert!(bubble.size.width > px(0.));
    }

    #[gpui_kit::test]
    fn attachments_over_the_receive_limit_are_left_out(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, NOTE_TO_SELF, 1, |_| true);
        let file = |name: &str, size: u64| AttachmentMeta {
            content_type: "application/zip".into(),
            file_name: Some(name.into()),
            size,
            width: None,
            height: None,
            local_path: None,
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        };
        let limit = signal_backend::remote_config().attachment_max_receive_bytes();
        let insert = |sent_at: i64, attachments: Vec<AttachmentMeta>| {
            let db = backend.db().clone();
            let message = signal_store::NewMessage {
                conversation_id: ConversationId(NOTE_TO_SELF.into()),
                sent_at,
                received_at: sent_at,
                direction: Direction::Outgoing,
                author: None,
                author_name: None,
                body: String::new(),
                status: DeliveryStatus::Sent,
                quote: None,
                attachments,
                link_preview: None,
                expires_in: 0,
            };
            futures::executor::block_on(backend.spawn(async move { db.insert_message(&message).await })).unwrap()
        };
        let now = chrono::Utc::now().timestamp_millis();
        let alone = insert(now, vec![file("huge.zip", limit + 1)]);
        let mixed = insert(now + 1, vec![file("small.zip", 10), file("huge.zip", limit + 1)]);
        let fits = insert(now + 2, vec![file("edge.zip", limit)]);
        let message = |id: i64| {
            let db = backend.db().clone();
            futures::executor::block_on(backend.spawn(async move { db.message(id).await })).unwrap().unwrap()
        };
        assert_eq!(too_big_text(&message(alone)), Some("Attachment too large to display."));
        assert_eq!(too_big_text(&message(mixed)), Some("Some attachments are too large to display."));
        assert_eq!(too_big_text(&message(fits)), None, "the limit itself is allowed");

        let mut cx = open(cx, &backend, NOTE_TO_SELF);
        bounds(&mut cx, format!("too-big-{alone}"));
        bounds(&mut cx, format!("too-big-{mixed}"));
        assert!(cx.debug_bounds(Box::leak(format!("too-big-{fits}").into_boxed_str())).is_none());
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

    #[gpui_kit::test]
    fn backup_thumbnails_stand_in_until_the_file_comes(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let photos = wait_for_messages(&backend, NOTE_TO_SELF, 1, |m| !m.attachments.is_empty());
        // The demo's drawn photo stands in for a thumbnail.
        let thumbnail = photos[0].attachments[0].local_path.clone();
        assert!(thumbnail.is_some());
        let not_downloaded = |content_type: &str, width: u32, height: u32| AttachmentMeta {
            content_type: content_type.into(),
            file_name: None,
            size: 2_000_000,
            width: Some(width),
            height: Some(height),
            local_path: None,
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: thumbnail.clone(),
        };
        let db = backend.db().clone();
        let sent_at = chrono::Utc::now().timestamp_millis();
        let message = signal_store::NewMessage {
            conversation_id: ConversationId(NOTE_TO_SELF.into()),
            sent_at,
            received_at: sent_at,
            direction: Direction::Outgoing,
            author: None,
            author_name: None,
            body: "From the backup".into(),
            status: DeliveryStatus::Sent,
            quote: None,
            attachments: vec![not_downloaded("image/jpeg", 800, 600), not_downloaded("video/mp4", 600, 800)],
            link_preview: None,
            expires_in: 0,
        };
        let id = futures::executor::block_on(backend.spawn(async move { db.insert_message(&message).await })).unwrap();
        let mut cx = open(cx, &backend, NOTE_TO_SELF);
        // The image's box at its own proportions, not the file row.
        let image = bounds(&mut cx, format!("thumbnail-{id}-0"));
        assert_eq!((image.size.width, image.size.height), (px(300.), px(225.)));
        let video = bounds(&mut cx, format!("video-{id}-1"));
        assert_eq!((video.size.width, video.size.height), (px(300.), px(400.)));
        backend.shutdown();
    }

    const MAYA: &str = "d:00000000-0000-4000-8000-00000000a11c";
    /// The last message the demo seeds in Maya's chat: once it's there, her
    /// chat is all there (waiting for less raced the seeding).
    const MAYA_LAST: &str = "Let me know what you think!";

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
                voice: false,
                remote: None,
                thumbnail_path: None,
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
        wait_for_messages(&backend, MAYA, 1, |m| m.body == MAYA_LAST);
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
    fn reaction_buttons_appear_in_turn() {
        assert!(ease_out_expo(0.).abs() < 1e-4);
        assert!((ease_out_expo(1.) - 1.).abs() < 1e-4);
        // Expo out: most of the way there by a quarter of the time.
        assert!(ease_out_expo(0.25) > 0.8);
        // The seventh button waits 60ms of its 460ms, then ends with the rest.
        assert_eq!(reaction_appear(59. / 460., 6), 0.);
        assert!(reaction_appear(61. / 460., 6) > 0.);
        assert!((reaction_appear(1., 6) - 1.).abs() < 1e-4);
        assert!(reaction_appear(0.1, 0) > reaction_appear(0.1, 3));
    }

    #[gpui_kit::test]
    fn reaction_bar_sits_over_the_react_button(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let message = wait_for_messages(&backend, GROUP, 1, |m| m.direction == Direction::Outgoing).remove(0);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        view.update(&mut cx, |v, cx| {
            v.reaction_bar_for = Some(message.id);
            cx.notify();
        });
        // The actions stay up with it, and it is centered 4px above "react".
        let button = bounds(&mut cx, format!("react-{}", message.id));
        let bar = bounds(&mut cx, format!("reaction-bar-{}", message.id));
        assert!((bar.center().x - button.center().x).abs() < px(0.5), "{bar:?} vs {button:?}");
        assert_eq!(bar.bottom(), button.top() - px(4.), "{bar:?} vs {button:?}");
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
    fn the_signal_chat_is_read_only_with_its_button(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let signal = ConversationId::signal().0;
        let notes = wait_for_messages(&backend, &signal, 1, |m| m.cta.is_some());
        let note = notes.iter().find(|m| m.cta.is_some()).unwrap().id;
        let (mut cx, _view) = open_view(cx, &backend, &signal);
        // Desktop's donate button, under the note, and its hero at the top.
        bounds(&mut cx, format!("cta-{note}"));
        bounds(&mut cx, "signal-hero".into());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("emoji-button").is_none(), "no composer");
    }

    #[gpui_kit::test]
    fn clicking_a_quote_shows_the_quoted_message(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 1, |m| m.body == MAYA_LAST);
        // An old message far up the chat, and today's reply to it.
        let db = backend.db().clone();
        let (quoted, reply) = futures::executor::block_on(backend.spawn(async move {
            let message = |n: i64, body: String| signal_store::NewMessage {
                conversation_id: ConversationId(MAYA.into()),
                sent_at: 1_000_000 + n * 60_000,
                received_at: 1_000_000 + n * 60_000,
                direction: Direction::Incoming,
                author: Some(MAYA.trim_start_matches("d:").into()),
                author_name: Some("Maya Chen".into()),
                body,
                status: DeliveryStatus::Read,
                quote: None,
                attachments: vec![],
                link_preview: None,
                expires_in: 0,
            };
            let mut quoted = 0;
            for n in 0..100i64 {
                let id = db.insert_message(&message(n, format!("old {n}"))).await?;
                if n == 5 {
                    quoted = id;
                }
            }
            let mut reply = message(0, "about that".into());
            reply.sent_at = chrono::Utc::now().timestamp_millis();
            reply.received_at = reply.sent_at;
            reply.quote = Some(signal_store::Quote {
                author: reply.author.clone(),
                author_name: "Maya Chen".into(),
                sent_at: 1_000_000 + 5 * 60_000,
                text: "old 5".into(),
                thumbnail: None,
                attachment: None,
                story: false,
            });
            Ok::<_, signal_store::Error>((quoted, db.insert_message(&reply).await?))
        }))
        .unwrap();
        let (mut cx, view) = open_view_sized(cx, &backend, MAYA, 600.);
        let quote = bounds(&mut cx, format!("quote-{reply}"));
        cx.simulate_click(quote.center(), gpui_kit::Modifiers::none());
        for _ in 0..300 {
            cx.run_until_parked();
            if view.read_with(&cx, |v, _| v.highlight == Some(quoted)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(view.read_with(&cx, |v, _| v.highlight), Some(quoted));
        let row = bounds(&mut cx, format!("row-{quoted}"));
        assert!(row.top() >= px(0.) && row.top() < px(600.), "on screen: {row:?}");
    }

    #[gpui_kit::test]
    fn jumping_to_a_search_result_and_back(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 1, |m| m.body == MAYA_LAST);
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
        // The demo's last note, so the chat is done being seeded (an earlier
        // one is a photo without text).
        wait_for_messages(&backend, NOTE_TO_SELF, 1, |m| m.body.starts_with("Notes from the hike"));
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
    fn consecutive_group_updates_fold_into_one_row(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let updates = wait_for_messages(&backend, GROUP, 3, |m| matches!(m.notice, Some(signal_store::Notice::Group { .. })));
        let (first, rest) = (updates[0].id, [updates[1].id, updates[2].id]);
        // Once the demo has read the chat (unread messages before them would
        // leave the updates unseen, shown as they are).
        wait_for_messages(&backend, GROUP, 1, |m| m.id < first && m.is_read);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        for _ in 0..300 {
            cx.run_until_parked();
            if view.read_with(&cx, |v, _| v.messages.iter().any(|m| m.id == first)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ix = view.read_with(&cx, |v, _| v.messages.iter().position(|m| m.id == first)).unwrap();
        view.update(&mut cx, |v, _| v.list.scroll_to(ListOffset { item_ix: ix, offset_in_item: px(0.) }));
        let button = bounds(&mut cx, format!("collapse-{first}"));
        assert!(button.size.height > px(0.));
        assert!(view.read_with(&cx, |v, _| matches!(v.slot(first), Slot::Head { count: 3, .. })));
        assert!(rest.iter().all(|id| view.read_with(&cx, |v, _| v.slot(*id) == Slot::Folded { set: first })));
        view.update(&mut cx, |v, cx| v.toggle_set(first, cx));
        assert!(view.read_with(&cx, |v, _| v.expanded.contains(&first)));
        view.update(&mut cx, |v, cx| v.toggle_set(first, cx));
        assert!(!view.read_with(&cx, |v, _| v.expanded.contains(&first)));
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
    fn a_voice_message_shows_its_waveform(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let voice = wait_for_messages(&backend, MAYA, 1, |m| m.attachments.first().is_some_and(|a| a.voice)).remove(0);
        let path = std::path::PathBuf::from(voice.attachments[0].local_path.clone().unwrap());
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        // Rendering the chat asks for the waveform; it's decoded in the
        // background (nothing is played).
        let mut wave = None;
        for _ in 0..300 {
            cx.run_until_parked();
            wave = state.update(&mut cx, |s, cx| s.waveform(&path, cx));
            if wave.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (peaks, seconds) = wave.expect("decoded");
        assert_eq!(peaks.len(), crate::audio::BARS);
        assert!((seconds - 6.).abs() < 0.1, "{seconds}");
        assert!(peaks.contains(&1.) && peaks.iter().any(|p| *p < 0.5), "it rises and falls");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn the_next_voice_message_plays_when_one_ends(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        // Maya's two voice messages, one after the other.
        let voices = wait_for_messages(&backend, MAYA, 2, |m| m.attachments.first().is_some_and(|a| a.voice));
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        // A silent player that reaches its end at once.
        state.update(&mut cx, |s, _| s.open_audio = crate::audio::silent::open);
        let path = std::path::PathBuf::from(voices[0].attachments[0].local_path.clone().unwrap());
        state.update(&mut cx, |s, cx| {
            s.toggle_audio(voices[0].id, 0, path.clone(), "Maya".into(), cx);
            s.cycle_audio_rate(cx);
        });
        let playing = |cx: &mut VisualTestContext| state.read_with(cx, |s, _| s.audio.as_ref().map(|a| (a.message_id, a.rate)));
        let mut next = None;
        for _ in 0..400 {
            cx.executor().advance_clock(std::time::Duration::from_millis(60));
            cx.run_until_parked();
            next = playing(&mut cx).filter(|(id, _)| *id == voices[1].id);
            if next.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (_, rate) = next.expect("the second one plays next");
        assert_eq!(rate, 1.5, "at the same rate");
        // Nothing after it: it stops.
        for _ in 0..400 {
            cx.executor().advance_clock(std::time::Duration::from_millis(60));
            cx.run_until_parked();
            if playing(&mut cx).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(playing(&mut cx), None);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn enter_sends_selected_text_rather_than_replacing_it(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 1, |_| true);
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        cx.update(|window, cx| {
            let composer = view.read(cx).composer.clone();
            composer.update(cx, |c, cx| c.focus(window, cx));
        });
        cx.simulate_input("all of it");
        cx.simulate_keystrokes("cmd-a enter");
        wait_for_messages(&backend, MAYA, 1, |m| m.body == "all of it" && m.direction == Direction::Outgoing);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn message_details_list_recipients_by_how_far_it_got(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let plan = wait_for_messages(&backend, GROUP, 1, |m| m.body.starts_with("Ridge loop it is")).remove(0);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        // Two read it, one has it, one it went to.
        let plan = wait_for_messages(&backend, GROUP, 1, |m| m.id == plan.id && m.status == DeliveryStatus::Read).remove(0);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        cx.update(|window, cx| crate::views::message_info::open(state.clone(), plan.clone(), window, cx));
        for header in ["Read by", "Delivered to", "Sent to"] {
            bounds(&mut cx, format!("details-{header}"));
        }
        assert!(cx.debug_bounds("details-Not sent").is_none());
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_changed_safety_number_is_confirmed_before_sending(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 1, |_| true);
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let state = view.read_with(&cx, |v, _| v.state.clone());
        let maya = MAYA.trim_start_matches("d:").to_owned();
        // We verified her, and her key changed since.
        let db = backend.db().clone();
        let who = maya.clone();
        futures::executor::block_on(backend.spawn(async move {
            db.set_setting(&format!("verified.{who}"), "aa").await.unwrap();
            db.set_setting(&format!("identity.{who}"), "bb").await.unwrap();
        }));
        for _ in 0..300 {
            state.update(&mut cx, |s, cx| s.reload_conversations(cx));
            cx.run_until_parked();
            let ready = state.read_with(&cx, |s, _| s.conversation(&ConversationId(MAYA.into())).is_some() && s.unverified.contains(&maya));
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.update(|window, cx| {
            let composer = view.read(cx).composer.clone();
            composer.update(cx, |c, cx| c.focus(window, cx));
        });
        cx.simulate_input("still there?");
        cx.simulate_keystrokes("enter");
        bounds(&mut cx, "safety-number-changes".into());
        let sent = |backend: &Backend| {
            let db = backend.db().clone();
            futures::executor::block_on(backend.spawn(async move { db.messages(&ConversationId(MAYA.into()), None, 50).await }))
                .unwrap()
                .iter()
                .any(|m| m.body == "still there?")
        };
        assert!(!sent(&backend), "nothing goes until it's confirmed");
        crate::views::test_support::click_at(&mut cx, "safety-changes-send");
        wait_for_messages(&backend, MAYA, 1, |m| m.body == "still there?" && m.direction == Direction::Outgoing);
        assert!(state.read_with(&cx, |s, _| !s.unverified.contains(&maya)));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn formatting_markers_are_sent_as_styles_beside_a_mention(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, GROUP, 1, |_| true);
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
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
        for _ in 0..100 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.simulate_input("hi @May");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.simulate_input("*bring* ||snacks||");
        cx.simulate_keystrokes("enter");
        let sent = wait_for_messages(&backend, GROUP, 1, |m| m.body.ends_with("snacks") && !m.styles.is_empty()).remove(0);
        assert_eq!(sent.body, "hi @Maya Chen bring snacks");
        let part = |s: &signal_store::StyleSpan| sent.body[s.start..s.start + s.len].to_owned();
        assert_eq!(sent.styles.iter().map(|s| (part(s), s.style)).collect::<Vec<_>>(), [
            ("bring".to_owned(), signal_store::TextStyle::Bold),
            ("snacks".to_owned(), signal_store::TextStyle::Spoiler),
        ]);
        assert_eq!(&sent.body[sent.mentions[0].start..sent.mentions[0].start + sent.mentions[0].len], "@Maya Chen");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn the_bold_shortcut_wraps_the_selection(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, MAYA, 1, |_| true);
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        cx.update(|_, cx| {
            cx.bind_keys([gpui_kit::KeyBinding::new("cmd-b", crate::views::composer::FormatBold, Some("Composer"))]);
        });
        cx.update(|window, cx| {
            let composer = view.read(cx).composer.clone();
            composer.update(cx, |c, cx| c.focus(window, cx));
        });
        cx.simulate_input("loud");
        // Enter right after: the word stays selected, and is sent, not
        // replaced with a line break.
        cx.simulate_keystrokes("cmd-a cmd-b enter");
        // (Its formatting is saved just after it.)
        let sent = wait_for_messages(&backend, MAYA, 1, |m| m.body == "loud" && !m.styles.is_empty()).remove(0);
        assert_eq!(sent.styles, [signal_store::StyleSpan { start: 0, len: 4, style: signal_store::TextStyle::Bold }]);
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
        backend.send(Command::CreateGroup { title: "Book club".into(), members: vec![MAYA.trim_start_matches("d:").into()], avatar: None, expire_timer: 0 });
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
        // Desktop's fixed size, however few the reactions.
        assert_eq!((viewer.size.width, viewer.size.height), (px(REACTION_VIEWER_WIDTH), px(REACTION_VIEWER_HEIGHT)));
        let tab = cx.update(|_, cx| view.read(cx).reaction_viewer.clone());
        assert_eq!(tab, Some((plan.id, None)));

        // A tab filters to one emoji.
        let thumbs = bounds(&mut cx, format!("reaction-tab-{}-0", plan.id));
        cx.simulate_click(thumbs.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        let tab = cx.update(|_, cx| view.read(cx).reaction_viewer.clone());
        assert_eq!(tab, Some((plan.id, Some("👍".into()))));

        // The pill that opened it closes it, and a click elsewhere does too.
        let viewer = |cx: &mut VisualTestContext| cx.update(|_, cx| view.read(cx).reaction_viewer.clone());
        cx.simulate_click(more.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(viewer(&mut cx), None);
        cx.simulate_click(more.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(viewer(&mut cx).is_some());
        cx.simulate_click(gpui_kit::point(px(450.), px(5.)), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(viewer(&mut cx), None);
        backend.shutdown();
    }

    #[test]
    fn unread_lines_and_badges_read_as_desktops() {
        assert_eq!(unread_line(1), "1 Unread Message");
        assert_eq!(unread_line(3), "3 Unread Messages");
        assert_eq!((badge_count(5), badge_count(99), badge_count(100)), ("5".into(), "99".into(), "99+".into()));
    }

    #[gpui_kit::test]
    fn an_unread_chat_opens_at_its_unread_line_and_mentions_wait_below(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        // The hiking group: two unread, the first mentioning us.
        let mention = wait_for_messages(&backend, GROUP, 1, |m| m.body.starts_with("@You")).remove(0);
        wait_for_messages(&backend, GROUP, 1, |m| m.body.contains("first aid kit"));
        let id = ConversationId(GROUP.into());
        // Opened once the chat list knows it's unread, as in the app.
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        let ready = signal_backend::Event::Ready { phone: Some("+1 555 0100".into()), aci: NOTE_TO_SELF.trim_start_matches("d:").into() };
        cx.update(|cx| state.update(cx, |s, cx| s.handle(ready, cx)));
        for _ in 0..300 {
            cx.run_until_parked();
            if cx.read(|cx| state.read(cx).conversation(&id).is_some_and(|c| c.unread_count == 2)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (window, chat) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(900.), px(400.)) })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let picker = cx.new(|cx| EmojiPicker::new(state.clone(), window, cx));
                cx.new(|cx| ConversationView::new(state.clone(), picker, id.clone(), window, cx))
            })
            .unwrap()
        });
        let mut cx = VisualTestContext::from_window(window, cx);
        let line = bounds(&mut cx, "unread-line".into());
        let (marker, count, mentions) =
            cx.update(|_, cx| (chat.read(cx).unseen_from, chat.read(cx).unread_at_open, chat.read(cx).unread_mentions.clone()));
        assert_eq!((marker, count), (Some(mention.id), 2));
        assert_eq!(mentions, [mention.id]);
        let row = bounds(&mut cx, format!("row-{}", mention.id));
        assert!(line.bottom() <= row.top(), "the line is above the oldest unread");
        // Reading it once found.
        wait_for_messages(&backend, GROUP, 1, |m| m.id == mention.id);
        let unread = || {
            let db = backend.db().clone();
            let id = id.clone();
            futures::executor::block_on(backend.spawn(async move { db.conversation(&id).await })).unwrap().unwrap().unread_count
        };
        for _ in 0..300 {
            if unread() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(unread(), 0);

        // Scrolled up: the date floats over the top, and "@" waits below.
        cx.update(|_, cx| {
            chat.update(cx, |v, cx| {
                v.list.set_follow_mode(FollowMode::Normal);
                v.list.scroll_to(ListOffset { item_ix: 1, offset_in_item: px(0.) });
                cx.notify();
            })
        });
        for _ in 0..50 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            if !chat.read_with(&cx, |v, _| v.at_end()) {
                break;
            }
        }
        cx.update(|_, cx| chat.update(cx, |v, cx| v.note_scrolled(cx)));
        bounds(&mut cx, "floating-date".into());
        // It goes three seconds after the scroll.
        cx.executor().advance_clock(FLOATING_DATE_FOR + std::time::Duration::from_millis(100));
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("floating-date").is_none());
        let at = bounds(&mut cx, "jump-to-mention".into());
        cx.simulate_click(at.center(), gpui_kit::Modifiers::none());
        crate::views::test_support::wait(&mut cx, "the jump", &mut |cx| {
            cx.update(|_, cx| chat.read(cx).highlight == Some(mention.id))
        });
        assert!(cx.update(|_, cx| chat.read(cx).unread_mentions.is_empty()));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn edited_opens_the_edit_history(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        // Our plan in the hiking group was edited twice.
        let plan = wait_for_messages(&backend, GROUP, 1, |m| m.edited && m.direction == Direction::Outgoing).remove(0);
        let (mut cx, _view) = open_view(cx, &backend, GROUP);
        let edited = bounds(&mut cx, format!("edited-{}", plan.id));
        cx.simulate_click(edited.center(), gpui_kit::Modifiers::none());
        // The message now, then its two earlier versions.
        bounds(&mut cx, "edit-history".into());
        let current = bounds(&mut cx, "edit-version-0".into());
        let oldest = bounds(&mut cx, "edit-version-2".into());
        assert!(oldest.top() > current.top(), "newest first");
        assert!(cx.debug_bounds("edit-version-3").is_none());
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn a_failed_delete_for_everyone_is_retried(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        // The demo's message to Maya whose delete didn't go.
        let failed = wait_for_messages(&backend, MAYA, 1, |m| m.deleted && m.status == DeliveryStatus::Failed).remove(0);
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let can_retry = cx.update(|_, cx| view.read(cx).can_retry_delete(&failed, cx));
        assert!(can_retry);
        let label = bounds(&mut cx, format!("delete-failed-{}", failed.id));
        cx.simulate_click(label.center(), gpui_kit::Modifiers::none());
        crate::views::test_support::press(&mut cx, "ok");
        let retried = wait_for_messages(&backend, MAYA, 1, |m| m.id == failed.id && m.status == DeliveryStatus::Sent);
        assert!(retried[0].deleted);
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

    const HOUSE: &str = "g:0000000000000000000000000000000000000000000000000000000000000003";

    /// Waits for the demo to have changed a message as `done` says, then
    /// tells the view, as `main.rs` would.
    fn changed(cx: &mut VisualTestContext, view: &Entity<ConversationView>, backend: &Backend, chat: &str, done: impl Fn(&Message) -> bool) -> Message {
        let message = wait_for_messages(backend, chat, 1, done).remove(0);
        let state = view.read_with(cx, |v, _| v.state.clone());
        let event = signal_backend::Event::MessageChanged { conversation: ConversationId(chat.into()), message_id: message.id };
        state.update(cx, |state, cx| state.handle(event, cx));
        cx.run_until_parked();
        message
    }

    #[gpui_kit::test]
    fn voting_in_a_poll_counts_and_changes_our_vote(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let poll = wait_for_messages(&backend, HOUSE, 1, |m| m.poll.is_some()).remove(0);
        let id = poll.id;
        let (mut cx, view) = open_view(cx, &backend, HOUSE);
        // The demo's: Leo chose Tuesday and Sunday, Ana Sunday.
        bounds(&mut cx, format!("poll-{id}"));
        assert_eq!(poll.poll.as_ref().unwrap().tally().count(2), 2);
        bounds(&mut cx, format!("poll-{id}-count-2"));

        let tuesday = bounds(&mut cx, format!("poll-{id}-option-0"));
        cx.simulate_click(tuesday.center(), gpui_kit::Modifiers::none());
        // While it sends, Tuesday (the option it changes) has Desktop's
        // spinner; Sunday, unchanged, doesn't.
        changed(&mut cx, &view, &backend, HOUSE, |m| m.poll.as_ref().is_some_and(|p| p.our_pending_vote().is_some()));
        bounds(&mut cx, format!("poll-{id}-sending-0"));
        assert!(cx.debug_bounds(Box::leak(format!("poll-{id}-sending-2").into_boxed_str())).is_none());
        // Sent: counted, and no longer pending (a pending vote counts too).
        let voted = changed(&mut cx, &view, &backend, HOUSE, |m| {
            m.poll.as_ref().is_some_and(|p| !p.tally().ours.is_empty() && p.our_pending_vote().is_none())
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds(Box::leak(format!("poll-{id}-sending-0").into_boxed_str())).is_none(), "sent: no spinner");
        let tally = voted.poll.unwrap().tally();
        assert_eq!(tally.ours, [0].into());
        assert_eq!((tally.count(0), tally.unique_voters), (2, 3));

        // Multiple choice: Sunday too; then Tuesday off again.
        let sunday = bounds(&mut cx, format!("poll-{id}-option-2"));
        cx.simulate_click(sunday.center(), gpui_kit::Modifiers::none());
        changed(&mut cx, &view, &backend, HOUSE, |m| m.poll.as_ref().is_some_and(|p| p.tally().ours.len() == 2));
        let tuesday = bounds(&mut cx, format!("poll-{id}-option-0"));
        cx.simulate_click(tuesday.center(), gpui_kit::Modifiers::none());
        let last = changed(&mut cx, &view, &backend, HOUSE, |m| m.poll.as_ref().is_some_and(|p| p.tally().ours == [2].into()));
        let poll = last.poll.unwrap();
        let ours: Vec<u32> = poll.votes.iter().filter(|v| v.author.is_none()).map(|v| v.vote_count).collect();
        assert_eq!(ours, [3], "one vote of ours, the third");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn our_poll_can_be_ended_and_says_so(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        wait_for_messages(&backend, GROUP, 1, |_| true);
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        backend.send(Command::SendPoll {
            conversation: ConversationId(GROUP.into()),
            question: " Start time? ".into(),
            options: vec!["7:30".into(), " ".into(), "8:30".into(), String::new()],
            allow_multiple: false,
        });
        let sent = changed(&mut cx, &view, &backend, GROUP, |m| m.poll.is_some());
        let poll = sent.poll.clone().unwrap();
        assert_eq!(sent.body, "Start time?");
        assert_eq!((poll.options.clone(), poll.allow_multiple), (vec!["7:30".to_owned(), "8:30".to_owned()], false));
        let id = sent.id;
        bounds(&mut cx, format!("poll-{id}"));
        // Single choice: picking 8:30 after 7:30 moves the vote.
        let first = bounds(&mut cx, format!("poll-{id}-option-0"));
        cx.simulate_click(first.center(), gpui_kit::Modifiers::none());
        changed(&mut cx, &view, &backend, GROUP, |m| m.poll.as_ref().is_some_and(|p| p.tally().ours == [0].into()));
        let second = bounds(&mut cx, format!("poll-{id}-option-1"));
        cx.simulate_click(second.center(), gpui_kit::Modifiers::none());
        changed(&mut cx, &view, &backend, GROUP, |m| m.poll.as_ref().is_some_and(|p| p.tally().ours == [1].into()));

        backend.send(Command::EndPoll { message_id: id });
        let ended = changed(&mut cx, &view, &backend, GROUP, |m| m.poll.as_ref().is_some_and(|p| p.ended));
        assert_eq!(crate::views::poll::status_text(ended.poll.as_ref().unwrap()), "Poll · Final results");
        let notice = changed(&mut cx, &view, &backend, GROUP, |m| matches!(m.notice, Some(signal_store::Notice::PollEnded { .. })));
        let text = crate::views::notices::notice_text(notice.notice.as_ref().unwrap(), |_| "?".into());
        assert_eq!(text, "You ended the poll: Start time?");
        // Ended: no more voting.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds(Box::leak(format!("poll-{id}-option-0").into_boxed_str())).is_none());
        backend.shutdown();
    }

    #[test]
    fn view_once_bubbles_say_what_desktops_do() {
        let photo = |local: bool| AttachmentMeta {
            content_type: "image/jpeg".into(),
            file_name: None,
            size: 2048,
            width: None,
            height: None,
            local_path: local.then(|| "/x.jpg".into()),
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        };
        let label = |v, outgoing, a: Option<&AttachmentMeta>| {
            let (_, title, detail) = view_once_label(v, outgoing, a);
            (title, detail)
        };
        assert_eq!(label(ViewOnce::Ready, false, Some(&photo(true))), ("View Once Photo", Some("Click to view".into())));
        assert_eq!(label(ViewOnce::Ready, false, Some(&photo(false))).1, Some(crate::views::common::format_size(2048)));
        let video = AttachmentMeta { content_type: "video/mp4".into(), ..photo(true) };
        assert_eq!(label(ViewOnce::Ready, false, Some(&video)).0, "View Once Video");
        assert_eq!(label(ViewOnce::Viewed, false, None), ("Viewed", None));
        assert_eq!(label(ViewOnce::Gone, false, None), ("View once media is not available", None));
        assert_eq!(label(ViewOnce::Invalid, false, None), ("Can't download view once media", None));
        // Ours, whatever became of it.
        assert_eq!(label(ViewOnce::Gone, true, None), ("View Once Media", None));
    }

    #[gpui_kit::test]
    fn a_view_once_photo_opens_once(cx: &mut TestAppContext) {
        let (backend, dir) = demo_backend(cx);
        let once = wait_for_messages(&backend, MAYA, 1, |m| m.view_once.is_some()).remove(0);
        assert_eq!(once.view_once, Some(ViewOnce::Ready));
        let original = std::path::PathBuf::from(once.attachments[0].local_path.clone().unwrap());
        assert!(original.exists());
        let (mut cx, view) = open_view(cx, &backend, MAYA);
        let bubble = bounds(&mut cx, format!("bubble-{}", once.id));
        bounds(&mut cx, format!("view-once-label-{}", once.id));
        cx.simulate_click(bubble.center(), gpui_kit::Modifiers::none());
        let viewed = changed(&mut cx, &view, &backend, MAYA, |m| m.id == once.id && m.view_once == Some(ViewOnce::Viewed));
        // Erased: the media is gone, and only the viewer's copy is left.
        assert!(viewed.attachments.is_empty());
        assert!(!original.exists());
        let copies: Vec<_> = std::fs::read_dir(dir.path().join("view-once")).unwrap().collect();
        assert_eq!(copies.len(), 1);
        // A second click does nothing: there's nothing left to open.
        let bubble = bounds(&mut cx, format!("bubble-{}", once.id));
        cx.simulate_click(bubble.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(std::fs::read_dir(dir.path().join("view-once")).unwrap().count(), 1);
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn votes_in_our_poll_are_unread_until_the_chat_is_read(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        // The group's seeding is done once its last message is in.
        wait_for_messages(&backend, GROUP, 1, |m| m.body.contains("first aid kit"));
        let chat = |backend: &Backend| {
            let db = backend.db().clone();
            futures::executor::block_on(backend.spawn(async move { db.conversation(&ConversationId(GROUP.into())).await }))
                .unwrap()
                .unwrap()
        };
        let unread_before = chat(&backend).unread_count;
        backend.send(Command::SendPoll {
            conversation: ConversationId(GROUP.into()),
            question: "Snacks?".into(),
            options: vec!["Chips".into(), "Fruit".into()],
            allow_multiple: false,
        });
        // The demo's member votes a moment later: unread, though not an
        // unread message.
        let mut unread = false;
        for _ in 0..300 {
            if chat(&backend).unread_poll_votes {
                unread = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(unread, "the vote never came");
        assert_eq!(chat(&backend).unread_count, unread_before);
        // Opening the chat reads it.
        let (mut cx, view) = open_view(cx, &backend, GROUP);
        cx.update(|_, cx| view.update(cx, |v, _| v.window_active = true));
        // The chat list as the backend's event would load it (this test
        // doesn't pump backend events into the state).
        let state = view.read_with(&cx, |v, _| v.state.clone());
        cx.update(|_, cx| state.update(cx, |s, cx| s.handle(signal_backend::Event::ConversationsChanged, cx)));
        let mut read = false;
        for _ in 0..300 {
            // Drawn, so the list sees the newest messages on screen.
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            if !chat(&backend).unread_poll_votes {
                read = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(read, "opening the chat didn't read the vote");
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn arrow_keys_move_a_targeted_message_that_enter_opens(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let photos = wait_for_messages(&backend, NOTE_TO_SELF, 3, |m| !m.attachments.is_empty());
        cx.update(crate::shortcuts::bind_keys);
        let (mut cx, view) = open_view(cx, &backend, NOTE_TO_SELF);
        let cx = &mut cx;
        let opened = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|_, cx| {
            let opened = opened.clone();
            let state = view.read(cx).state.clone();
            cx.subscribe(&state, move |_, event: &AppEvent, _| {
                if let AppEvent::ViewImage(_) = event {
                    opened.set(opened.get() + 1);
                }
            })
            .detach();
        });
        let targeted = |cx: &mut VisualTestContext| view.read_with(cx, |v, _| v.targeted);

        // Clicking a message targets it (beside the bubble, not on the photo).
        let row = bounds(cx, format!("row-{}", photos[2].id));
        cx.simulate_click(row.origin + gpui_kit::point(px(8.), px(8.)), gpui_kit::Modifiers::none());
        assert_eq!(targeted(cx), Some(photos[2].id));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.timeline_focused));
        bounds(cx, format!("targeted-{}", photos[2].id));
        assert_eq!(opened.get(), 0, "clicking beside a photo doesn't open it");

        // ↑ and ↓ move it through the timeline, one item at a time.
        let ids: Vec<i64> = view.read_with(cx, |v, _| v.messages.iter().map(|m| m.id).collect());
        let at = ids.iter().position(|&id| id == photos[2].id).unwrap();
        cx.simulate_keystrokes("up");
        assert_eq!(targeted(cx), Some(ids[at - 1]));
        cx.simulate_keystrokes("down down");
        assert_eq!(targeted(cx), ids.get(at + 1).copied().or(Some(ids[at])));
        // Home: the first loaded; End: the newest.
        cx.simulate_keystrokes("home");
        assert_eq!(targeted(cx), Some(ids[0]));
        cx.simulate_keystrokes("cmd-down");
        assert_eq!(targeted(cx), ids.last().copied());

        // Enter on a photo opens it full-window.
        let row = bounds(cx, format!("row-{}", photos[0].id));
        cx.simulate_click(row.origin + gpui_kit::point(px(8.), px(8.)), gpui_kit::Modifiers::none());
        cx.simulate_keystrokes("enter");
        assert_eq!(opened.get(), 1);

        // ⌘⇧E's reactions and ⌘⇧R's reply, for the targeted message.
        cx.update(|_, cx| view.update(cx, |v, cx| v.toggle_reactions_for_targeted(cx)));
        assert_eq!(view.read_with(cx, |v, _| v.reaction_bar_for), Some(photos[0].id));
        cx.update(|window, cx| view.update(cx, |v, cx| v.toggle_reply_to_targeted(window, cx)));
        let replying = |cx: &mut VisualTestContext| view.read_with(cx, |v, cx| v.composer.read(cx).replying_to());
        assert_eq!(replying(cx), Some(photos[0].id));
        cx.update(|window, cx| view.update(cx, |v, cx| v.toggle_reply_to_targeted(window, cx)));
        assert_eq!(replying(cx), None, "pressed again, the reply goes");

        // Esc: back to typing.
        cx.update(|window, cx| {
            let handle = view.read(cx).timeline_focus.clone();
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(targeted(cx), None);
        assert!(cx.update(|window, cx| view.read(cx).composer.read(cx).focus_handle(cx).is_focused(window)));
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn command_j_targets_the_oldest_unread_message(cx: &mut TestAppContext) {
        let (backend, _dir) = demo_backend(cx);
        let messages = wait_for_messages(&backend, NOTE_TO_SELF, 3, |m| !m.attachments.is_empty());
        let (mut cx, view) = open_view(cx, &backend, NOTE_TO_SELF);
        let cx = &mut cx;
        bounds(cx, format!("row-{}", messages[0].id));
        // Nothing unread: the last message.
        cx.update(|window, cx| view.update(cx, |v, cx| v.target_oldest_unread_or_last(window, cx)));
        let last = view.read_with(cx, |v, _| v.messages.last().map(|m| m.id));
        assert_eq!(view.read_with(cx, |v, _| v.targeted), last);
        // Two were unread when it opened: the older of the last two incoming.
        let incoming: Vec<i64> = view.read_with(cx, |v, _| {
            v.messages.iter().filter(|m| m.notice.is_none() && m.direction == Direction::Incoming).map(|m| m.id).collect()
        });
        if incoming.len() >= 2 {
            view.update(cx, |v, _| v.unread_at_open = 2);
            cx.update(|window, cx| view.update(cx, |v, cx| v.target_oldest_unread_or_last(window, cx)));
            assert_eq!(view.read_with(cx, |v, _| v.targeted), Some(incoming[incoming.len() - 2]));
        }
        backend.shutdown();
    }
}
