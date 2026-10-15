//! Stories: receiving them, as Desktop does (`MessageReceiver`'s
//! `#handleStoryMessage`, `ts/util/shouldDownloadStory`,
//! `ts/state/ducks/stories`' `markStoryRead`), keeping their replies and
//! views, and letting them go after a day.

use presage::proto::{ReceiptMessage, SyncMessage, receipt_message, sync_message};
use prost::Message as _;
use signal_store::{NewStory, StoryReply};

use super::*;

/// Desktop's `MAX_NUM_STORIES_TO_PREFETCH`: someone's stories download on
/// their own while they have this many or fewer.
const MAX_STORIES_TO_PREFETCH: usize = 5;
use crate::{SETTING_STORIES_DISABLED, SETTING_STORY_VIEW_RECEIPTS};

impl Actor {
    async fn stories_disabled(&self) -> bool {
        self.db.setting(SETTING_STORIES_DISABLED).await.ok().flatten().is_some_and(|v| v == "true")
    }

    /// Desktop's `areStoryViewReceiptsEnabled`: its own setting, else the
    /// read receipts one.
    async fn story_view_receipts(&self) -> bool {
        match self.db.setting(SETTING_STORY_VIEW_RECEIPTS).await.ok().flatten() {
            Some(value) => value != "false",
            None => self.setting_enabled(SETTING_READ_RECEIPTS).await,
        }
    }

    /// Whether we'd see their story: Desktop shows one-to-one stories only
    /// from people it shares its profile with; here, someone we have a
    /// chat with or in our contacts, not blocked. A group's, only from a
    /// member of a group we're in.
    async fn takes_story_from(&self, author: &str, group: Option<&ConversationId>) -> bool {
        if self.is_blocked(&ConversationId::direct(author)).await {
            return false;
        }
        match group {
            Some(group) => self
                .db
                .conversation(group)
                .await
                .ok()
                .flatten()
                .is_some_and(|g| !g.blocked && !g.left && g.members.iter().any(|m| m == author)),
            None => {
                self.db.conversation(&ConversationId::direct(author)).await.ok().flatten().is_some()
                    || self.db.people().await.unwrap_or_default().iter().any(|p| p.id.as_direct() == Some(author))
            }
        }
    }

    pub(super) async fn apply_story(&self, manager: &SignalManager, mut story: NewStory, pointer: Option<AttachmentPointer>) {
        if self.stories_disabled().await {
            return;
        }
        if let Some(author) = story.author.clone() {
            if !self.takes_story_from(&author, story.group.as_ref()).await {
                debug!("story from someone we don't take stories from");
                return;
            }
            story.author_name = self.name_of_plain(&author).await;
        }
        story.pointer = pointer.as_ref().map(|p| p.encode_to_vec());
        let id = match self.db.insert_story(&story, now_millis()).await {
            Ok(Some(id)) => id,
            Ok(None) => return,
            Err(error) => return self.error("failed to keep a story", error),
        };
        self.story_expiry.notify_one();
        self.emit(Event::StoriesChanged);
        // Desktop's `shouldDownloadStory`: ours always; theirs while they
        // have only a few up.
        if let Some(pointer) = pointer {
            let theirs = match &story.author {
                None => 0,
                Some(author) => self
                    .db
                    .stories(now_millis())
                    .await
                    .map(|all| all.iter().filter(|s| s.author.as_deref() == Some(author)).count())
                    .unwrap_or(0),
            };
            let small = pointer.size.is_some_and(|size| u64::from(size) <= AUTO_DOWNLOAD_MAX);
            if small && (story.author.is_none() || theirs <= MAX_STORIES_TO_PREFETCH) {
                self.download_story(manager, id, &pointer).await;
            }
        }
    }

    /// A name without the `Rc` (stories come in through `&self`).
    async fn name_of_plain(&self, service_id: &str) -> Option<String> {
        self.db.conversation(&ConversationId::direct(service_id)).await.ok().flatten().map(|c| c.title).filter(|t| !t.is_empty())
    }

    async fn download_story(&self, manager: &SignalManager, id: i64, pointer: &AttachmentPointer) {
        let saved = async {
            let bytes = manager.get_attachment(pointer).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let path = self.config.data_dir.join("attachments").join(format!("story-{id}.{}", extension_for(pointer)));
            tokio::fs::write(&path, &bytes).await?;
            self.db.set_story_file(id, &path.to_string_lossy()).await?;
            anyhow::Ok(())
        };
        match saved.await {
            Ok(()) => self.emit(Event::StoriesChanged),
            Err(error) => warn!(error = %format!("{error:#}"), "couldn't download a story"),
        }
    }

    /// Downloads a story's photo or video (when it's opened).
    pub(super) async fn fetch_story(&self, id: i64) {
        let Some(manager) = self.manager() else { return };
        let pointer = match self.db.story_pointer(id).await {
            Ok(Some(bytes)) => AttachmentPointer::decode(bytes.as_slice()).ok(),
            _ => None,
        };
        let Some(pointer) = pointer else { return };
        self.download_story(&manager, id, &pointer).await;
    }

    pub(super) async fn apply_story_reply(
        &self,
        group: ConversationId,
        story_author: Option<String>,
        story_sent_at: i64,
        mut reply: StoryReply,
    ) {
        if self.stories_disabled().await {
            return;
        }
        if let Some(author) = &reply.author {
            reply.author_name = self.name_of_plain(author).await;
        }
        match self.db.add_story_reply(story_author.as_deref(), story_sent_at, &reply).await {
            Ok(Some(_)) => self.emit(Event::StoriesChanged),
            // Desktop drops a group reply to a story it doesn't have.
            Ok(None) => debug!(%group, "reply to a story we don't have"),
            Err(error) => self.error("failed to keep a story reply", error),
        }
    }

    /// A VIEWED receipt: counted on our story, if it's one (and we share
    /// view receipts, as Desktop only counts them then).
    pub(super) async fn apply_story_viewed(&self, viewer: String, sent_at: Vec<i64>) {
        if !self.story_view_receipts().await {
            return;
        }
        let mut changed = false;
        for at in sent_at {
            changed |= self.db.add_story_view(at, &viewer).await.unwrap_or(false);
        }
        if changed {
            self.emit(Event::StoriesChanged);
        }
    }

    /// We viewed these on another device.
    pub(super) async fn apply_viewed_sync(&self, viewed: Vec<(Option<String>, i64)>) {
        let mut changed = false;
        for (author, sent_at) in viewed {
            changed |= self.db.mark_story_read(author.as_deref(), sent_at).await.unwrap_or(false);
        }
        if changed {
            self.emit(Event::StoriesChanged);
        }
    }

    /// Desktop's `markStoryRead`: viewed here, on our other devices, and
    /// (with view receipts on) told to its author.
    pub(super) async fn view_story(&self, id: i64) {
        let Ok(Some(story)) = self.db.story(id).await else { return };
        let Some(author) = story.author.clone() else { return };
        match self.db.mark_story_read(Some(&author), story.sent_at).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => return self.error("failed to mark a story viewed", error),
        }
        self.emit(Event::StoriesChanged);
        let mut sync = SyncMessage::with_padding(&mut rand::rng());
        sync.viewed = vec![sync_message::Viewed {
            sender_aci: Some(author.clone()),
            timestamp: Some(story.sent_at as u64),
            ..Default::default()
        }];
        self.enqueue(Outbound::Sync { body: sync });
        if self.story_view_receipts().await {
            let receipt =
                ReceiptMessage { r#type: Some(receipt_message::Type::Viewed.into()), timestamp: vec![story.sent_at as u64] };
            self.enqueue(Outbound::Send {
                conversation: ConversationId::direct(&author),
                body: receipt.into(),
                timestamp: self.next_timestamp() as u64,
                message: None,
            });
        }
    }

    /// Lets stories go when their day is up.
    pub(super) async fn story_expiry_loop(self: Rc<Self>) {
        loop {
            match self.db.remove_expired_stories(now_millis()).await {
                Ok(files) => {
                    for file in &files {
                        let _ = tokio::fs::remove_file(file).await;
                    }
                    if !files.is_empty() {
                        self.emit(Event::StoriesChanged);
                    }
                }
                Err(error) => warn!(error = %format!("{error:#}"), "couldn't remove expired stories"),
            }
            let wait = match self.db.next_story_expiry().await.ok().flatten() {
                Some(at) => Duration::from_millis((at - now_millis()).clamp(0, 60 * 60 * 1000) as u64 + 50),
                None => Duration::from_secs(60 * 60),
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = self.story_expiry.notified() => {}
            }
        }
    }

    /// Desktop's reply (or reaction) to someone's story, one to one: a
    /// message in your chat with them, carrying the story it answers
    /// (`storyContext`), shown with Desktop's "Story" quote.
    pub(super) async fn reply_to_story(&self, id: i64, body: String, emoji: Option<String>) {
        let Ok(Some(story)) = self.db.story(id).await else { return };
        let Some(author) = story.author.clone() else { return };
        if !story.allows_replies {
            return self.error("can't reply", "this story doesn't take replies");
        }
        if story.group.is_some() {
            // A group reply goes to the group as a story send, which the
            // sender here can't do yet.
            return self.error("can't reply", "replies to group stories can't be sent yet");
        }
        let body = match &emoji {
            Some(emoji) => emoji.clone(),
            None => body.trim().to_owned(),
        };
        if body.is_empty() {
            return;
        }
        let conversation = ConversationId::direct(&author);
        let timestamp = self.next_timestamp();
        let thumbnail = match &story.content {
            signal_store::StoryContent::Media { attachment, .. } if attachment.content_type.starts_with("image/") => {
                attachment.local_path.clone()
            }
            _ => None,
        };
        let quote = signal_store::Quote {
            author: Some(author.clone()),
            author_name: story.author_name.clone().unwrap_or_default(),
            sent_at: story.sent_at,
            text: if emoji.is_some() { "Reacted to a story".into() } else { "Story".into() },
            thumbnail,
            attachment: None,
            story: true,
        };
        let message = NewMessage {
            conversation_id: conversation.clone(),
            sent_at: timestamp,
            received_at: timestamp,
            direction: Direction::Outgoing,
            author: None,
            author_name: None,
            body: body.clone(),
            status: DeliveryStatus::Sending,
            quote: Some(quote),
            attachments: Vec::new(),
            link_preview: None,
            expires_in: 0,
        };
        let id = match self.db.insert_message(&message).await {
            Ok(id) => id,
            Err(error) => return self.error("failed to store the reply", error),
        };
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
        self.emit(Event::ConversationsChanged);
        let context = presage::proto::data_message::StoryContext {
            author_aci: Some(author.clone()),
            sent_timestamp: Some(story.sent_at as u64),
            ..Default::default()
        };
        let data = DataMessage {
            body: emoji.is_none().then_some(body),
            timestamp: Some(timestamp as u64),
            story_context: Some(context),
            reaction: emoji.map(|emoji| presage::proto::data_message::Reaction {
                emoji: Some(emoji),
                remove: Some(false),
                target_author_aci: Some(author),
                target_sent_timestamp: Some(story.sent_at as u64),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp: timestamp as u64, message: Some(id) });
    }

    /// Hides (or shows again) a person's or group's stories, here.
    pub(super) async fn set_story_hidden(&self, chat: &ConversationId, hidden: bool) -> signal_store::Result<()> {
        self.db.set_story_hidden(chat, hidden).await?;
        self.emit(Event::StorySettingsChanged);
        Ok(())
    }

    /// Stories' settings from Storage Service: the story lists, hidden
    /// stories, stories off, and view receipts (Desktop syncs all four).
    pub(super) async fn save_story_settings(
        &self,
        lists: Vec<crate::StoryList>,
        hidden: Vec<ConversationId>,
        disabled: Option<bool>,
        view_receipts: Option<bool>,
    ) {
        let saved = async {
            self.db.set_setting(crate::SETTING_STORY_LISTS, &serde_json::to_string(&lists)?).await?;
            let mut hidden: Vec<String> = hidden.into_iter().map(|c| c.0).collect();
            hidden.sort();
            self.db.set_setting(crate::SETTING_HIDDEN_STORIES, &serde_json::to_string(&hidden)?).await?;
            if let Some(disabled) = disabled {
                self.db.set_setting(SETTING_STORIES_DISABLED, if disabled { "true" } else { "false" }).await?;
                if disabled {
                    for file in self.db.remove_all_stories().await? {
                        let _ = tokio::fs::remove_file(file).await;
                    }
                }
            }
            if let Some(on) = view_receipts {
                self.db.set_setting(SETTING_STORY_VIEW_RECEIPTS, if on { "true" } else { "false" }).await?;
            }
            anyhow::Ok(())
        };
        match saved.await {
            Ok(()) => {
                self.emit(Event::StorySettingsChanged);
                self.emit(Event::StoriesChanged);
            }
            Err(error) => warn!(error = %format!("{error:#}"), "couldn't save story settings"),
        }
    }
}
