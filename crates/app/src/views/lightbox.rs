//! What the photo and video viewers share as Desktop's lightbox
//! (`ts/components/Lightbox.dom.tsx`): stepping through the chat's photos
//! and videos (the arrows, and ← →), who sent the one shown and when,
//! Forward, and going to its message.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, Hsla, SharedString, Window, div, px};
use signal_store::{ConversationId, Direction, Message};

use crate::state::AppState;

/// Desktop loads the lightbox's media in pages; the newest few hundred
/// messages with media cover most chats.
const LIMIT: u32 = 500;

/// A photo or video in its chat.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaItem {
    pub message_id: i64,
    pub index: usize,
    pub path: PathBuf,
    pub video: bool,
    /// A photo's pixel size, when known.
    pub size: Option<(u32, u32)>,
    pub file_name: Option<String>,
    pub content_type: String,
    pub sent_at: i64,
    /// Who sent it ("You" for ours).
    pub sender: String,
}

/// Where a viewed photo or video sits: its chat, its message and which
/// attachment, and once loaded the chat's media to step through.
#[derive(Debug, Clone, PartialEq)]
pub struct InChat {
    pub conversation: ConversationId,
    pub message_id: i64,
    pub index: usize,
    pub gallery: Option<Rc<Vec<MediaItem>>>,
}

impl InChat {
    pub fn new(conversation: ConversationId, message_id: i64, index: usize) -> Self {
        Self { conversation, message_id, index, gallery: None }
    }

    /// Where the one shown is in the gallery.
    pub fn position(&self) -> Option<usize> {
        let gallery = self.gallery.as_ref()?;
        gallery.iter().position(|item| item.message_id == self.message_id && item.index == self.index)
    }

    /// The one `by` steps away (−1 the one before), if there is one.
    pub fn step(&self, by: isize) -> Option<MediaItem> {
        let at = self.position()?;
        let to = at.checked_add_signed(by)?;
        self.gallery.as_ref()?.get(to).cloned()
    }

    /// The item shown, once the gallery is loaded.
    pub fn current(&self) -> Option<&MediaItem> {
        self.gallery.as_ref()?.get(self.position()?)
    }
}

/// A chat's photos and videos, oldest first, as the lightbox steps through
/// them (Desktop's `media` items: images and videos, not stickers, GIFs,
/// view-once media, or a shared contact's photo). Only downloaded ones can
/// be shown.
pub fn gallery(newest_first: &[Message]) -> Vec<MediaItem> {
    let mut items = Vec::new();
    for message in newest_first.iter().rev() {
        // A shared contact, payment or gift isn't media.
        if message.deleted || message.view_once.is_some() || message.content.is_some() {
            continue;
        }
        let sender = match message.direction {
            Direction::Outgoing => "You".to_owned(),
            Direction::Incoming => message.author_name.clone().unwrap_or_else(|| "Unknown".into()),
        };
        for (index, attachment) in message.attachments.iter().enumerate() {
            let image = attachment.content_type.starts_with("image/");
            let video = attachment.content_type.starts_with("video/");
            if !(image || video) || attachment.sticker || attachment.gif {
                continue;
            }
            let Some(path) = &attachment.local_path else { continue };
            items.push(MediaItem {
                message_id: message.id,
                index,
                path: PathBuf::from(path),
                video,
                size: attachment.width.zip(attachment.height).filter(|(w, h)| *w > 0 && *h > 0),
                file_name: attachment.file_name.clone(),
                content_type: attachment.content_type.clone(),
                sent_at: message.sent_at,
                sender: sender.clone(),
            });
        }
    }
    items
}

/// Loads the chat's media for a viewer, then hands it over.
pub fn load(state: &Entity<AppState>, conversation: ConversationId, cx: &mut App, done: impl FnOnce(Vec<MediaItem>, &mut App) + 'static) {
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let query = backend.spawn(async move { db.media_messages(&conversation, LIMIT).await });
    cx.spawn(async move |cx| {
        match query.await {
            Ok(messages) => {
                let items = gallery(&messages);
                cx.update(|cx| done(items, cx));
            }
            Err(error) => tracing::warn!(%error, "couldn't load the chat's media"),
        }
    })
    .detach();
}

/// Desktop's Save (`saveAttachment` without a folder): a save dialog,
/// starting in Downloads with the file's name.
pub fn save_as(source: PathBuf, name: String, cx: &mut App) {
    let downloads = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_default();
    let chosen = cx.prompt_for_new_path(&downloads, Some(&name));
    cx.spawn(async move |_| {
        if let Ok(Ok(Some(path))) = chosen.await
            && let Err(error) = std::fs::copy(&source, &path)
        {
            tracing::warn!(%error, "failed to save an attachment");
        }
    })
    .detach();
}

/// Shows `item` in the viewer that suits it, in place of the one open.
pub fn show(state: &Entity<AppState>, in_chat: &InChat, item: &MediaItem, cx: &mut App) {
    let next = InChat { message_id: item.message_id, index: item.index, ..in_chat.clone() };
    state.update(cx, |state, cx| {
        if item.video {
            let video = crate::views::video_viewer::ViewedVideo {
                path: item.path.clone(),
                file_name: item.file_name.clone(),
                view_once: false,
                in_chat: Some(next),
            };
            state.view_video(video, cx);
        } else {
            let image = crate::views::image_viewer::ViewedImage {
                path: item.path.clone(),
                size: item.size,
                file_name: item.file_name.clone(),
                content_type: item.content_type.clone(),
                view_once: false,
                in_chat: Some(next),
            };
            state.view_image(image, cx);
        }
    });
}

/// Forward (Desktop's lightbox button): the lightbox closes first.
pub fn forward(state: &Entity<AppState>, in_chat: &InChat, window: &mut Window, cx: &mut App) {
    crate::views::forward::open(state.clone(), vec![in_chat.message_id], window, cx);
}

/// Its message in the chat (Desktop's All media "View in chat").
pub fn go_to(state: &Entity<AppState>, in_chat: &InChat, cx: &mut App) {
    let (conversation, message_id) = (in_chat.conversation.clone(), in_chat.message_id);
    state.update(cx, |state, cx| state.jump_to(conversation, message_id, cx));
}

/// Who sent it and when, as Desktop's `LightboxHeader`.
pub fn header(in_chat: Option<&InChat>, fallback: Option<&str>) -> impl IntoElement {
    let current = in_chat.and_then(InChat::current);
    let (title, subtitle): (SharedString, Option<SharedString>) = match current {
        Some(item) => (item.sender.clone().into(), Some(date_time(item.sent_at).into())),
        None => (fallback.unwrap_or_default().to_owned().into(), None),
    };
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(div().truncate().text_sm().child(title))
        .when_some(subtitle, |d, s| d.child(div().truncate().text_xs().opacity(0.7).child(s)))
}

/// "Oct 14, 2026, 3:04 PM": Desktop's `formatDateTimeForAttachment`.
fn date_time(millis: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|t| t.format("%b %-d, %Y, %-I:%M %p").to_string())
        .unwrap_or_default()
}

/// A round button over the media, as Desktop's previous and next.
pub fn nav_button(id: &'static str, icon: IconName, left: bool) -> gpui_kit::Stateful<gpui_kit::Div> {
    let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
    div()
        .id(id)
        .debug_selector(move || id.into())
        .absolute()
        .top_0()
        .bottom_0()
        .when(left, |d| d.left_4())
        .when(!left, |d| d.right_4())
        .flex()
        .items_center()
        .child(
            div()
                .size(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .bg(Hsla { a: 0.2, ..white })
                .hover(|d| d.bg(Hsla { a: 0.3, ..white }))
                .child(Icon::new(icon).size_6().text_color(white)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use signal_store::{AttachmentMeta, DeliveryStatus};

    fn attachment(content_type: &str, path: Option<&str>) -> AttachmentMeta {
        AttachmentMeta {
            content_type: content_type.into(),
            file_name: None,
            size: 1,
            width: Some(4),
            height: Some(3),
            local_path: path.map(Into::into),
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        }
    }

    fn message(id: i64, attachments: Vec<AttachmentMeta>) -> Message {
        Message {
            id,
            conversation_id: ConversationId("d:a".into()),
            sent_at: id * 10,
            received_at: id * 10,
            direction: Direction::Incoming,
            author: Some("a".into()),
            author_name: Some("Ana".into()),
            body: String::new(),
            status: DeliveryStatus::Delivered,
            reactions: vec![],
            quote: None,
            attachments,
            link_preview: None,
            deleted: false,
            edited: false,
            expires_in: 0,
            expire_start: None,
            edit_count: 0,
            notice: None,
            mentions: vec![],
            styles: vec![],
            cta: None,
            deleted_by_admin: None,
            long_text: None,
            poll: None,
            view_once: None,
            sealed_sender: vec![],
            content: None,
            is_read: true,
        }
    }

    #[gpui_kit::test]
    fn the_lightbox_steps_through_the_chats_photos_and_goes_to_the_message(cx: &mut gpui_kit::TestAppContext) {
        use crate::views::test_support::{click_at, demo_root, shows, wait};
        let (backend, state, mut cx, _dir) = demo_root(cx);
        let cx = &mut cx;
        // Note to Self: three photos of ours.
        let me = ConversationId("d:00000000-0000-4000-8000-000000000001".into());
        let photos = {
            let db = backend.db().clone();
            let me = me.clone();
            let mut found = Vec::new();
            for _ in 0..300 {
                let (db, me) = (db.clone(), me.clone());
                found = futures::executor::block_on(backend.spawn(async move { db.media_messages(&me, 50).await })).unwrap_or_default();
                if found.len() >= 3 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            gallery(&found)
        };
        assert_eq!(photos.len(), 3);
        let first = &photos[0];
        let image = crate::views::image_viewer::ViewedImage {
            path: first.path.clone(),
            size: first.size,
            file_name: None,
            content_type: first.content_type.clone(),
            view_once: false,
            in_chat: Some(InChat::new(me.clone(), first.message_id, first.index)),
        };
        cx.update(|_, cx| state.update(cx, |s, cx| s.view_image(image, cx)));
        // The oldest: nothing before it.
        wait(cx, "the next button", &mut |cx| shows(cx, "lightbox-next"));
        assert!(!shows(cx, "lightbox-previous"));
        click_at(cx, "lightbox-next");
        wait(cx, "the second photo", &mut |cx| shows(cx, "lightbox-previous"));
        // Go to message: the viewer closes and the chat opens at it.
        crate::views::test_support::press(cx, "go-to-image-message");
        wait(cx, "the chat", &mut |cx| {
            !shows(cx, "viewed-image") && state.read_with(cx, |s, _| s.selected.as_ref() == Some(&me))
        });
        backend.shutdown();
    }

    #[test]
    fn the_gallery_steps_through_photos_and_videos_oldest_first() {
        let mut gif = attachment("video/mp4", Some("/g"));
        gif.gif = true;
        // Newest first, as the store gives them.
        let messages = [
            message(4, vec![attachment("video/mp4", Some("/v"))]),
            message(3, vec![gif, attachment("audio/aac", Some("/a"))]),
            message(2, vec![attachment("image/png", None), attachment("image/png", Some("/p2"))]),
            message(1, vec![attachment("image/jpeg", Some("/p1"))]),
        ];
        let items = gallery(&messages);
        assert_eq!(items.iter().map(|i| (i.message_id, i.index, i.video)).collect::<Vec<_>>(), [(1, 0, false), (2, 1, false), (4, 0, true)]);

        let mut in_chat = InChat::new(ConversationId("d:a".into()), 2, 1);
        assert_eq!(in_chat.step(1), None, "not loaded yet");
        in_chat.gallery = Some(Rc::new(items));
        assert_eq!(in_chat.position(), Some(1));
        assert_eq!(in_chat.step(1).map(|i| i.message_id), Some(4));
        assert_eq!(in_chat.step(-1).map(|i| i.message_id), Some(1));
        assert_eq!(in_chat.step(2), None);
        assert_eq!(in_chat.current().map(|i| i.sender.as_str()), Some("Ana"));
    }
}
