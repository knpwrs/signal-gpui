//! Shared contacts, payment notifications and gift badges after they're
//! stored: a contact's photo downloaded as Desktop's `queueAttachmentDownloads`
//! queues it (a photo, behind the auto-download choices), a gift badge's
//! badge looked up in the subscription configuration (Desktop's
//! `handleDataMessage`, cached for an hour as `getCachedSubscriptionConfiguration`),
//! and a gift badge viewed on the phone marked redeemed or opened
//! (`ts/messageModifiers/ViewSyncs.preload.ts`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use signal_store::{Badge, MessageContent};

use super::*;

/// Desktop's `SUBSCRIPTION_CONFIG_CACHE_TIME`.
const CONFIG_CACHE: Duration = Duration::from_secs(60 * 60);

thread_local! {
    /// The levels' badges, and when they were fetched (the actor's thread).
    static LEVEL_BADGES: RefCell<Option<(Instant, HashMap<u64, Badge>)>> = const { RefCell::new(None) };
}

impl Actor {
    /// What follows storing a message's content: its photo or badge.
    pub(super) fn content_stored(
        self: &Rc<Self>,
        manager: &SignalManager,
        conversation: &ConversationId,
        message_id: i64,
        content: &MessageContent,
        image: Option<AttachmentPointer>,
        held: bool,
    ) {
        match content {
            MessageContent::Contact(_) => {
                let Some(pointer) = image.filter(|p| {
                    !held
                        && crate::auto_download::current().photos
                        && p.size.is_some_and(|size| crate::remote_config().auto_downloads(u64::from(size)))
                }) else {
                    return;
                };
                let (this, manager, conversation) = (self.clone(), manager.clone(), conversation.clone());
                tokio::task::spawn_local(async move {
                    this.download_contact_avatar(&manager, conversation, message_id, &pointer).await
                });
            }
            MessageContent::GiftBadge(gift) if gift.badge_id.is_none() => {
                let (this, manager, conversation, level) = (self.clone(), manager.clone(), conversation.clone(), gift.level);
                tokio::task::spawn_local(async move { this.name_gift_badge(&manager, conversation, message_id, level).await });
            }
            _ => {}
        }
    }

    async fn download_contact_avatar(
        &self,
        manager: &SignalManager,
        conversation: ConversationId,
        message_id: i64,
        pointer: &AttachmentPointer,
    ) {
        let bytes = match manager.get_attachment(pointer).await {
            Ok(bytes) => bytes,
            Err(error) => return debug!(%error, "failed to download a shared contact's photo"),
        };
        let path = self
            .config
            .data_dir
            .join("attachments")
            .join(format!("{message_id}-contact.{}", extension_for(pointer)));
        if let Err(error) = tokio::fs::write(&path, &bytes).await {
            return self.error("failed to save a shared contact's photo", error);
        }
        match self.db.set_contact_avatar_path(message_id, &path.to_string_lossy()).await {
            Ok(true) => self.emit(Event::MessageChanged { conversation, message_id }),
            Ok(false) => {}
            Err(error) => self.error("failed to record a shared contact's photo", error),
        }
    }

    /// Looks up the badge a gift badge's level stands for, saves it with
    /// the others, and names it on the message. One fetch an hour at most;
    /// a failed one is tried again with the next gift badge.
    async fn name_gift_badge(self: Rc<Self>, manager: &SignalManager, conversation: ConversationId, message_id: i64, level: u64) {
        let cached = LEVEL_BADGES.with_borrow(|cache| {
            cache.as_ref().filter(|(at, _)| at.elapsed() < CONFIG_CACHE).map(|(_, badges)| badges.get(&level).cloned())
        });
        let badge = match cached {
            Some(badge) => badge,
            None => match crate::profiles::fetch_level_badges(manager).await {
                Ok(badges) => {
                    let badge = badges.get(&level).cloned();
                    LEVEL_BADGES.with_borrow_mut(|cache| *cache = Some((Instant::now(), badges)));
                    badge
                }
                Err(error) => return warn!(error = format!("{error:#}"), "couldn't fetch the subscription configuration"),
            },
        };
        let Some(badge) = badge else {
            return warn!(level, "gift badge level not found on server");
        };
        if let Err(error) = self.db.save_badges(std::slice::from_ref(&badge)).await {
            return warn!(%error, "failed to save a gift badge");
        }
        match self.db.set_gift_badge_id(message_id, &badge.id).await {
            Ok(_) => {
                self.download_badges();
                self.emit(Event::BadgesChanged);
                self.emit(Event::MessageChanged { conversation, message_id });
            }
            Err(error) => self.error("failed to name a gift badge", error),
        }
    }

    /// Messages viewed on another of our devices: a gift badge among them
    /// is redeemed (theirs) or opened (ours).
    pub(super) async fn gift_badges_viewed(&self, viewed: &[(Option<String>, i64)]) {
        for (author, sent_at) in viewed {
            let Ok(Some(message)) = self.db.find_message_any(author.as_deref(), *sent_at).await else { continue };
            if !matches!(message.content, Some(MessageContent::GiftBadge(_))) {
                continue;
            }
            let incoming = message.direction == Direction::Incoming;
            match self.db.gift_badge_viewed(message.id, incoming).await {
                Ok(true) => {
                    self.emit(Event::MessageChanged { conversation: message.conversation_id.clone(), message_id: message.id });
                    self.emit(Event::ConversationsChanged);
                }
                Ok(false) => {}
                Err(error) => self.error("failed to mark a gift badge viewed", error),
            }
        }
    }
}
