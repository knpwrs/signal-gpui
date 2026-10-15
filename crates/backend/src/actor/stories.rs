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
            let size = pointer.size.map_or(0, u64::from);
            if size > crate::MAX_RECEIVE_ATTACHMENT_BYTES {
                anyhow::bail!("{size} bytes is over Signal's receive limit");
            }
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
        let now = now_millis();
        for at in sent_at {
            changed |= self.db.add_story_view(at, &viewer, now).await.unwrap_or(false);
        }
        if changed {
            self.emit(Event::StoriesChanged);
        }
    }

    /// Another of our devices changed who our story goes to (Desktop's
    /// `onStoryRecipientUpdate`): those left without a list lose it, and
    /// with no one left it's gone.
    pub(super) async fn apply_story_recipient_update(&self, sent_at: i64, recipients: Vec<signal_store::StoryRecipient>) {
        let Ok(Some(story)) = self.db.find_story(None, sent_at).await else { return };
        if story.group.is_some() {
            return;
        }
        let remaining: Vec<_> = recipients.into_iter().filter(|r| !r.list_ids.is_empty()).collect();
        let done = if remaining.is_empty() {
            self.forget_story(story.id).await
        } else {
            self.db.set_story_recipients(story.id, &remaining).await.map_err(Into::into)
        };
        match done {
            Ok(()) => self.emit(Event::StoriesChanged),
            Err(error) => warn!(error = %format!("{error:#}"), "couldn't update a story's recipients"),
        }
    }

    /// Someone deleted their story for everyone (Desktop's `Deletes`
    /// finds stories too). Returns whether it was one.
    pub(super) async fn apply_story_delete(&self, author: Option<&str>, sent_at: i64) -> bool {
        let Ok(Some(story)) = self.db.find_story(author, sent_at).await else { return false };
        match self.forget_story(story.id).await {
            Ok(()) => self.emit(Event::StoriesChanged),
            Err(error) => warn!(error = %format!("{error:#}"), "couldn't remove a deleted story"),
        }
        true
    }

    /// Removes a story here, with its file.
    async fn forget_story(&self, id: i64) -> anyhow::Result<()> {
        if let Some(file) = self.db.remove_story(id).await? {
            let _ = tokio::fs::remove_file(file).await;
        }
        Ok(())
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
        let body = match &emoji {
            Some(emoji) => emoji.clone(),
            None => body.trim().to_owned(),
        };
        if body.is_empty() {
            return;
        }
        if let Some(group) = story.group.clone() {
            return self.reply_to_group_story(&story, &author, group, body, emoji).await;
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

    /// Stories' settings, from Storage Service or changed here (`None`:
    /// unchanged): the story lists, hidden stories, stories off, view
    /// receipts, and whether My Story's viewers were chosen (Desktop syncs
    /// them all).
    pub(super) async fn save_story_settings(
        &self,
        lists: Option<Vec<crate::StoryList>>,
        hidden: Option<Vec<ConversationId>>,
        disabled: Option<bool>,
        view_receipts: Option<bool>,
        my_story_privacy_set: Option<bool>,
    ) {
        let saved = async {
            if let Some(lists) = lists {
                self.db.set_setting(crate::SETTING_STORY_LISTS, &serde_json::to_string(&lists)?).await?;
            }
            if let Some(hidden) = hidden {
                let mut hidden: Vec<String> = hidden.into_iter().map(|c| c.0).collect();
                hidden.sort();
                self.db.set_setting(crate::SETTING_HIDDEN_STORIES, &serde_json::to_string(&hidden)?).await?;
            }
            if let Some(set) = my_story_privacy_set {
                self.db.set_setting(crate::SETTING_MY_STORY_PRIVACY_SET, if set { "true" } else { "false" }).await?;
            }
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

    /// A reply to a group's story goes to the group's members as a story
    /// send (Desktop sends it with the envelope's story flag), and shows
    /// with the story.
    async fn reply_to_group_story(
        &self,
        story: &signal_store::Story,
        author: &str,
        group: ConversationId,
        body: String,
        emoji: Option<String>,
    ) {
        let Some(context) = self.group_context(&group).await else { return };
        let timestamp = self.next_timestamp();
        let reply = StoryReply { author: None, author_name: None, sent_at: timestamp, body: body.clone(), emoji: emoji.clone() };
        if let Err(error) = self.db.add_story_reply(Some(author), story.sent_at, &reply).await {
            return self.error("failed to keep the reply", error);
        }
        self.emit(Event::StoriesChanged);
        let data = DataMessage {
            body: emoji.is_none().then_some(body),
            timestamp: Some(timestamp as u64),
            group_v2: Some(context),
            story_context: Some(presage::proto::data_message::StoryContext {
                author_aci: Some(author.to_owned()),
                sent_timestamp: Some(story.sent_at as u64),
                ..Default::default()
            }),
            reaction: emoji.map(|emoji| presage::proto::data_message::Reaction {
                emoji: Some(emoji),
                remove: Some(false),
                target_author_aci: Some(author.to_owned()),
                target_sent_timestamp: Some(story.sent_at as u64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let members = self.group_story_members(&group).await;
        let parts = members.into_iter().map(|m| (m, ContentBody::DataMessage(data.clone()))).collect();
        self.start_story_job(parts, timestamp as u64, StoryJobSync::Reply(Box::new(data)), "reply");
    }

    /// The members of a group to send its story to: everyone but us.
    async fn group_story_members(&self, group: &ConversationId) -> Vec<String> {
        let me = self.self_aci();
        self.db
            .conversation(group)
            .await
            .ok()
            .flatten()
            .map(|g| g.members.into_iter().filter(|m| m != &me).collect())
            .unwrap_or_default()
    }

    /// The story lists, from Storage Service (and our edits since).
    pub(super) async fn story_lists(&self) -> Vec<crate::StoryList> {
        self.db
            .setting(crate::SETTING_STORY_LISTS)
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// Who a story list sends to (Desktop's `sendStoryMessage`): My Story
    /// as a block list goes to all Signal connections (people we have a
    /// chat with or in our contacts) but those it names ("All except…");
    /// any other list, to the people it names. Never us or the Signal chat.
    async fn list_members(&self, list: &crate::StoryList) -> Vec<String> {
        let me = self.self_aci();
        let mut people: Vec<String> = if list.is_my_story() && list.block_list {
            self.db
                .people()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| p.id.as_direct().map(str::to_owned))
                .filter(|sid| !list.members.contains(sid))
                .collect()
        } else {
            list.members.clone()
        };
        people.retain(|p| p != &me && p != signal_store::SIGNAL_ACI);
        people.sort();
        people.dedup();
        people
    }

    /// Desktop's `sendStoryMessage`: the photo or video uploaded once; the
    /// story lists' members sent it once each, under one timestamp; each
    /// group its own. Recipient by recipient in the outbound queue, and a
    /// transcript to our other devices when done.
    pub(super) async fn post_story(&self, story: crate::OutgoingStory, targets: Vec<crate::StoryTarget>) {
        let Some(manager) = self.manager() else { return self.error("can't post the story", "not connected") };
        if targets.is_empty() {
            return;
        }
        let base = self.next_timestamp();
        // The attachment: uploaded once for every target.
        let (attachment, local) = match &story {
            crate::OutgoingStory::Text(text) => (presage::proto::story_message::Attachment::TextAttachment(text_attachment(text)), None),
            crate::OutgoingStory::Media { bytes, content_type, caption } => {
                match self.upload_story_media(&manager, bytes, content_type, caption, base).await {
                    Ok((pointer, meta)) => (presage::proto::story_message::Attachment::FileAttachment(pointer), Some(meta)),
                    Err(error) => return self.error("couldn't post the story", format!("{error:#}")),
                }
            }
        };
        let content = match (&story, &local) {
            (crate::OutgoingStory::Text(text), _) => signal_store::StoryContent::Text((**text).clone()),
            (crate::OutgoingStory::Media { caption, .. }, Some(meta)) => {
                signal_store::StoryContent::Media { attachment: meta.clone(), caption: caption.clone(), styles: Vec::new() }
            }
            (crate::OutgoingStory::Media { .. }, None) => return,
        };
        let all_lists = self.story_lists().await;
        let mut lists = Vec::new();
        let mut groups = Vec::new();
        for target in targets {
            match target {
                crate::StoryTarget::MyStory => {
                    lists.push(all_lists.iter().find(|l| l.is_my_story()).cloned().unwrap_or_else(crate::StoryList::my_story));
                }
                crate::StoryTarget::List(id) => match all_lists.iter().find(|l| l.id == id) {
                    Some(list) => lists.push(list.clone()),
                    None => self.error("couldn't post to a story", "that story list is gone"),
                },
                crate::StoryTarget::Group(group) => groups.push(group),
            }
        }
        if !lists.is_empty() {
            let mut resolved = Vec::new();
            for list in lists {
                let members = self.list_members(&list).await;
                resolved.push((list, members));
            }
            let recipients = list_recipients(&resolved);
            let allows_replies = recipients.iter().any(|r| r.allows_replies) || resolved.iter().any(|(l, _)| l.allows_replies);
            let message = presage::proto::StoryMessage {
                attachment: Some(attachment.clone()),
                allows_replies: Some(allows_replies),
                ..Default::default()
            };
            self.keep_our_story(base, None, content.clone(), allows_replies, recipients.clone()).await;
            info!(recipients = recipients.len(), lists = resolved.len(), "posting a story");
            let parts = recipients
                .iter()
                .map(|r| {
                    let message = presage::proto::StoryMessage { allows_replies: Some(r.allows_replies), ..message.clone() };
                    (r.service_id.clone(), ContentBody::StoryMessage(message))
                })
                .collect();
            self.start_story_job(parts, base as u64, StoryJobSync::Story { message: Box::new(message), recipients }, "story");
        }
        for group in groups {
            // Its own timestamp, so a later send never takes it.
            let timestamp = self.next_timestamp();
            let message = presage::proto::StoryMessage {
                group: self.group_context(&group).await,
                attachment: Some(attachment.clone()),
                allows_replies: Some(true),
                ..Default::default()
            };
            let recipients: Vec<signal_store::StoryRecipient> = self
                .group_story_members(&group)
                .await
                .into_iter()
                .map(|service_id| signal_store::StoryRecipient { service_id, list_ids: Vec::new(), allows_replies: true })
                .collect();
            self.keep_our_story(timestamp, Some(group), content.clone(), true, Vec::new()).await;
            info!(recipients = recipients.len(), "posting a group story");
            let parts = recipients.iter().map(|r| (r.service_id.clone(), ContentBody::StoryMessage(message.clone()))).collect();
            self.start_story_job(parts, timestamp as u64, StoryJobSync::Story { message: Box::new(message), recipients }, "story");
        }
        self.story_expiry.notify_one();
        self.emit(Event::StoriesChanged);
    }

    async fn keep_our_story(
        &self,
        sent_at: i64,
        group: Option<ConversationId>,
        content: signal_store::StoryContent,
        allows_replies: bool,
        recipients: Vec<signal_store::StoryRecipient>,
    ) {
        let ours = NewStory {
            author: None,
            author_name: None,
            sent_at,
            received_at: sent_at,
            group,
            content,
            pointer: None,
            allows_replies,
            read: true,
            recipients,
        };
        if let Err(error) = self.db.insert_story(&ours, now_millis()).await {
            self.error("failed to keep the story", error);
        }
    }

    /// Desktop's `deleteStoryForEveryone`, for our story: a group's is
    /// deleted as a group message; otherwise everyone who loses it (all
    /// its recipients, or with `list`, those only that list sent it to)
    /// gets a delete as a story, and our other devices a recipient update.
    pub(super) async fn delete_story_for_everyone(&self, story: &signal_store::Story, list: Option<&str>) {
        if story.author.is_some() {
            return;
        }
        let timestamp = self.next_timestamp();
        let data = DataMessage {
            timestamp: Some(timestamp as u64),
            delete: Some(presage::proto::data_message::Delete { target_sent_timestamp: Some(story.sent_at as u64) }),
            ..Default::default()
        };
        if let Some(group) = &story.group {
            if list.is_some() {
                return;
            }
            let data = DataMessage { group_v2: self.group_context(group).await, ..data };
            self.enqueue(Outbound::Send { conversation: group.clone(), body: data.into(), timestamp: timestamp as u64, message: None });
            if let Err(error) = self.forget_story(story.id).await {
                warn!(error = %format!("{error:#}"), "couldn't remove a deleted story");
            }
            return;
        }
        if story.recipients.is_empty() {
            // Posted before we kept who stories went to.
            warn!("deleting a story without knowing who it went to; only here");
        }
        let (lost, updated) = without_list(&story.recipients, list);
        let remaining: Vec<_> = updated.iter().filter(|r| !r.list_ids.is_empty()).cloned().collect();
        let done = if remaining.is_empty() {
            self.forget_story(story.id).await
        } else {
            self.db.set_story_recipients(story.id, &remaining).await.map_err(Into::into)
        };
        if let Err(error) = done {
            warn!(error = %format!("{error:#}"), "couldn't remove a deleted story");
        }
        info!(recipients = lost.len(), "deleting a story for everyone");
        let parts = lost.into_iter().map(|r| (r, ContentBody::DataMessage(data.clone()))).collect();
        self.start_story_job(
            parts,
            timestamp as u64,
            StoryJobSync::RecipientUpdate { sent_at: story.sent_at as u64, recipients: updated },
            "deletion",
        );
    }

    /// Desktop's "Turn off stories" (its words: "Story updates you have
    /// recently shared will also be deleted"), or on again: here, then in
    /// the account record.
    pub(super) async fn set_stories_disabled(self: Rc<Self>, disabled: bool) {
        if disabled {
            // Ours go for everyone first, while we know who has them.
            for story in self.db.stories(now_millis()).await.unwrap_or_default() {
                if story.author.is_none() {
                    self.delete_story_for_everyone(&story, None).await;
                }
            }
        }
        self.save_story_settings(None, None, Some(disabled), None, None).await;
        self.write_account(&[storage::AccountEdit::StoriesDisabled(disabled)]).await;
    }

    /// Story view receipts, here and in the account record.
    pub(super) async fn set_story_view_receipts(self: Rc<Self>, on: bool) {
        self.save_story_settings(None, None, None, Some(on), None).await;
        self.write_account(&[storage::AccountEdit::StoryViewReceipts(on)]).await;
    }

    /// Writes story settings to the account record; if that fails,
    /// re-reading Storage Service puts things back.
    async fn write_account(self: &Rc<Self>, edits: &[storage::AccountEdit]) {
        let Some(manager) = self.manager() else { return };
        match storage::write_account(&manager, edits).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't update your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// Desktop's story list edits (`storyDistributionLists`): saved here,
    /// then to Storage Service. Setting My Story's viewers also marks them
    /// chosen (`hasSetMyStoriesPrivacy`).
    pub(super) async fn save_story_list(self: Rc<Self>, list: crate::StoryList) {
        {
            let _edit = self.story_lists_edit.lock().await;
            let mut lists = self.story_lists().await;
            match lists.iter_mut().find(|l| l.id == list.id) {
                Some(existing) => *existing = list.clone(),
                None => lists.push(list.clone()),
            }
            let privacy_set = list.is_my_story().then_some(true);
            self.save_story_settings(Some(lists), None, None, None, privacy_set).await;
        }
        let Some(manager) = self.manager() else { return };
        let written = async {
            storage::write_story_list(&manager, &list, 0).await?;
            if list.is_my_story() {
                storage::write_account(&manager, &[storage::AccountEdit::HasSetMyStoriesPrivacy(true)]).await?;
            }
            anyhow::Ok(())
        };
        match written.await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't save the story's privacy on your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// Desktop's `deleteDistributionList`: gone here, marked deleted in
    /// Storage Service, and the stories sent to it deleted for everyone.
    pub(super) async fn delete_story_list(self: Rc<Self>, id: String) {
        if id == crate::MY_STORY_ID {
            return;
        }
        let list = {
            let _edit = self.story_lists_edit.lock().await;
            let mut lists = self.story_lists().await;
            let Some(at) = lists.iter().position(|l| l.id == id) else { return };
            let list = lists.remove(at);
            self.save_story_settings(Some(lists), None, None, None, None).await;
            list
        };
        for story in self.db.stories(now_millis()).await.unwrap_or_default() {
            if story.author.is_none() && story.recipients.iter().any(|r| r.list_ids.contains(&id)) {
                self.delete_story_for_everyone(&story, Some(&id)).await;
            }
        }
        self.emit(Event::StoriesChanged);
        let Some(manager) = self.manager() else { return };
        match storage::write_story_list(&manager, &list, now_millis() as u64).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't delete the story on your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// Uploads a story's photo (optimized, as photos in chats are) or video
    /// (as it is), keeping a copy to show.
    async fn upload_story_media(
        &self,
        manager: &SignalManager,
        bytes: &[u8],
        content_type: &str,
        caption: &str,
        timestamp: i64,
    ) -> anyhow::Result<(AttachmentPointer, AttachmentMeta)> {
        let (bytes, content_type, size) = if content_type.starts_with("image/") {
            let level = self.image_optimization().await;
            let original = bytes.to_vec();
            let prepared = tokio::task::spawn_blocking(move || media::prepare(&original, level)).await??;
            (prepared.bytes, prepared.content_type.to_owned(), Some((prepared.width, prepared.height)))
        } else {
            (bytes.to_vec(), content_type.to_owned(), None)
        };
        let ext = extension_of(&content_type);
        let path = self.config.data_dir.join("attachments").join(format!("story-sent-{timestamp}.{ext}"));
        tokio::fs::write(&path, &bytes).await?;
        let spec = AttachmentSpec {
            content_type: content_type.clone(),
            length: bytes.len(),
            file_name: None,
            preview: None,
            voice_note: None,
            borderless: None,
            width: size.map(|s| s.0),
            height: size.map(|s| s.1),
            caption: Some(caption.to_owned()).filter(|c| !c.is_empty()),
            blur_hash: None,
        };
        let length = bytes.len();
        let pointer = manager
            .upload_attachment(spec, bytes)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("uploading: {e}"))?;
        let meta = AttachmentMeta {
            content_type,
            file_name: None,
            size: length as u64,
            width: size.map(|s| s.0),
            height: size.map(|s| s.1),
            local_path: Some(path.to_string_lossy().into_owned()),
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
        };
        Ok((pointer, meta))
    }

    /// Queues a story send to each recipient; when the last is done, sends
    /// the transcript to our other devices and says who it didn't reach.
    fn start_story_job(&self, parts: Vec<(String, ContentBody)>, timestamp: u64, sync: StoryJobSync, what: &'static str) {
        let job = self.next_story_job.get();
        self.next_story_job.set(job + 1);
        let parts: Vec<(String, ContentBody)> =
            parts.into_iter().filter(|(r, _)| ServiceId::parse_from_service_id_string(r).is_some()).collect();
        self.story_jobs.borrow_mut().insert(
            job,
            StoryJob { remaining: parts.len(), timestamp, sync, sent: Vec::new(), failures: HashMap::new(), what },
        );
        if parts.is_empty() {
            // No one to send to: still tell our other devices.
            self.finish_story_job(job);
            return;
        }
        for (recipient, body) in parts {
            self.enqueue(Outbound::Story { recipient, body, timestamp, job });
        }
    }

    pub(super) async fn send_story_part(
        &self,
        manager: &mut SignalManager,
        recipient: String,
        body: ContentBody,
        timestamp: u64,
        job: u64,
    ) {
        let result = match ServiceId::parse_from_service_id_string(&recipient) {
            Some(service_id) => manager.send_story(service_id, body, timestamp).await.map_err(|e| failure_reason(&format!("{e}"))),
            None => Err("not a Signal account".to_owned()),
        };
        self.story_part_done(job, recipient, result);
    }

    /// One recipient's send finished (or couldn't start).
    pub(super) fn story_part_done(&self, job: u64, recipient: String, result: Result<(), String>) {
        let done = {
            let mut jobs = self.story_jobs.borrow_mut();
            let Some(state) = jobs.get_mut(&job) else { return };
            match result {
                Ok(()) => state.sent.push(recipient),
                Err(reason) => *state.failures.entry(reason).or_default() += 1,
            }
            state.remaining = state.remaining.saturating_sub(1);
            state.remaining == 0
        };
        if done {
            self.finish_story_job(job);
        }
    }

    fn finish_story_job(&self, job: u64) {
        let Some(state) = self.story_jobs.borrow_mut().remove(&job) else { return };
        let total = state.sent.len() + state.failures.values().sum::<usize>();
        if !state.failures.is_empty() {
            let mut reasons: Vec<String> = state.failures.iter().map(|(reason, n)| format!("{n} {reason}")).collect();
            reasons.sort();
            warn!(sent = state.sent.len(), total, ?reasons, what = state.what, "story send incomplete");
            self.error(
                &format!("the {} reached {} of {total}", state.what, state.sent.len()),
                reasons.join(", "),
            );
        } else {
            info!(sent = state.sent.len(), what = state.what, "story sent");
        }
        let transcript = |r: &signal_store::StoryRecipient| sync_message::sent::StoryMessageRecipient {
            destination_service_id: Some(r.service_id.clone()),
            distribution_list_ids: r.list_ids.clone(),
            is_allowed_to_reply: Some(r.allows_replies),
            ..Default::default()
        };
        let sent = match state.sync {
            StoryJobSync::Story { message, recipients } => sync_message::Sent {
                timestamp: Some(state.timestamp),
                story_message: Some(*message),
                // Those it reached.
                story_message_recipients: recipients.iter().filter(|r| state.sent.contains(&r.service_id)).map(transcript).collect(),
                ..Default::default()
            },
            StoryJobSync::Reply(data) => sync_message::Sent {
                timestamp: Some(state.timestamp),
                message: Some(*data),
                ..Default::default()
            },
            // Desktop's `sendDeleteStoryForEveryone` sync: the story's own
            // timestamp, sent to us, everyone's lists now.
            StoryJobSync::RecipientUpdate { sent_at, recipients } => sync_message::Sent {
                destination_service_id: Some(self.self_aci()),
                timestamp: Some(sent_at),
                story_message_recipients: recipients.iter().map(transcript).collect(),
                is_recipient_update: Some(true),
                ..Default::default()
            },
        };
        let mut sync = SyncMessage::with_padding(&mut rand::rng());
        sync.content = Some(sync_message::Content::Sent(sent));
        self.enqueue(Outbound::Sync { body: sync });
    }
}

/// What the transcript to our other devices carries.
pub(super) enum StoryJobSync {
    /// The story, and who it's for (the transcript names those reached).
    Story { message: Box<presage::proto::StoryMessage>, recipients: Vec<signal_store::StoryRecipient> },
    Reply(Box<DataMessage>),
    /// A story deleted for some or all: who has which lists now.
    RecipientUpdate { sent_at: u64, recipients: Vec<signal_store::StoryRecipient> },
}

/// Desktop's `sendStoryMessage` for story lists: everyone on any of them,
/// once, with the lists that send it to them, allowed to reply if any of
/// those lists allows replies.
fn list_recipients(lists: &[(crate::StoryList, Vec<String>)]) -> Vec<signal_store::StoryRecipient> {
    let mut out: Vec<signal_store::StoryRecipient> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (list, members) in lists {
        for member in members {
            match index.get(member.as_str()) {
                Some(&at) => {
                    let recipient = &mut out[at];
                    recipient.list_ids.push(list.id.clone());
                    recipient.allows_replies |= list.allows_replies;
                }
                None => {
                    index.insert(member, out.len());
                    out.push(signal_store::StoryRecipient {
                        service_id: member.clone(),
                        list_ids: vec![list.id.clone()],
                        allows_replies: list.allows_replies,
                    });
                }
            }
        }
    }
    out
}

/// Desktop's `deleteStoryForEveryone`: when `list` (`None`: every list)
/// stops sending a story, who loses it (those no other list sends it to),
/// and everyone's lists after.
fn without_list(
    recipients: &[signal_store::StoryRecipient],
    list: Option<&str>,
) -> (Vec<String>, Vec<signal_store::StoryRecipient>) {
    let mut lost = Vec::new();
    let mut updated = Vec::new();
    for recipient in recipients {
        let had = list.is_none_or(|l| recipient.list_ids.iter().any(|id| id == l));
        let list_ids: Vec<String> = match list {
            Some(l) => recipient.list_ids.iter().filter(|id| *id != l).cloned().collect(),
            None => Vec::new(),
        };
        if had && list_ids.is_empty() {
            lost.push(recipient.service_id.clone());
        }
        updated.push(signal_store::StoryRecipient { list_ids, ..recipient.clone() });
    }
    (lost, updated)
}

/// A story (or group story reply) going out recipient by recipient.
pub(super) struct StoryJob {
    remaining: usize,
    timestamp: u64,
    sync: StoryJobSync,
    sent: Vec<String>,
    /// Why some didn't get it, and how many each.
    failures: HashMap<String, usize>,
    what: &'static str,
}

/// A send failure, said shortly (as each reason is counted).
fn failure_reason(error: &str) -> String {
    let error = error.trim();
    if error.contains("not registered") || error.contains("NotFound") {
        "no longer on Signal".into()
    } else if error.contains("dentity") {
        "safety number changed".into()
    } else if error.contains("ate limit") || error.contains("429") {
        "rate limited".into()
    } else {
        error.chars().take(80).collect()
    }
}

/// A text story as Signal sends it (`TextAttachment`).
fn text_attachment(text: &signal_store::TextStory) -> presage::proto::TextAttachment {
    use presage::proto::text_attachment::{Background, Gradient, Style};
    use signal_store::{StoryBackground, StoryFont};
    presage::proto::TextAttachment {
        text: Some(text.text.clone()),
        text_style: Some(
            match text.font {
                StoryFont::Default => Style::Default,
                StoryFont::Regular => Style::Regular,
                StoryFont::Bold => Style::Bold,
                StoryFont::Serif => Style::Serif,
                StoryFont::Script => Style::Script,
                StoryFont::Condensed => Style::Condensed,
            }
            .into(),
        ),
        text_foreground_color: text.foreground,
        text_background_color: text.text_background,
        preview: None,
        background: Some(match &text.background {
            StoryBackground::Color(color) => Background::Color(*color),
            StoryBackground::Gradient { colors, positions, angle } => Background::Gradient(Gradient {
                start_color: colors.first().copied(),
                end_color: colors.last().copied(),
                angle: Some(*angle),
                colors: colors.clone(),
                positions: positions.clone(),
            }),
        }),
    }
}


/// A file extension for a story's media type.
fn extension_of(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(id: &str, replies: bool) -> crate::StoryList {
        crate::StoryList { id: id.into(), allows_replies: replies, ..Default::default() }
    }

    fn people(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn story_lists_send_once_to_each_member() {
        let recipients = list_recipients(&[
            (list("my", false), people(&["a", "b"])),
            (list("close", true), people(&["b", "c"])),
        ]);
        let got: Vec<_> = recipients.iter().map(|r| (r.service_id.as_str(), r.list_ids.join(","), r.allows_replies)).collect();
        // Desktop: allowed to reply when any list they're on allows it.
        assert_eq!(got, [("a", "my".into(), false), ("b", "my,close".into(), true), ("c", "close".into(), true)]);
    }

    #[test]
    fn deleting_a_list_takes_the_story_only_from_those_it_alone_sent_to() {
        let recipients = list_recipients(&[(list("my", true), people(&["a", "b"])), (list("close", true), people(&["b", "c"]))]);
        let (lost, updated) = without_list(&recipients, Some("close"));
        assert_eq!(lost, ["c"]);
        let lists: Vec<_> = updated.iter().map(|r| (r.service_id.as_str(), r.list_ids.join(","))).collect();
        assert_eq!(lists, [("a", "my".into()), ("b", "my".into()), ("c", String::new())], "everyone, for the recipient update");

        // The whole story: everyone loses it.
        let (lost, updated) = without_list(&recipients, None);
        assert_eq!(lost, ["a", "b", "c"]);
        assert!(updated.iter().all(|r| r.list_ids.is_empty()));
    }
}
