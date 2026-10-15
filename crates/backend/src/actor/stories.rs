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
        self.start_story_job(members, ContentBody::DataMessage(data.clone()), timestamp as u64, StoryJobSync::Reply(Box::new(data)), "reply");
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

    /// Who My Story goes to, and whether they may reply (Desktop's
    /// `StoriesSettingsModal`): all Signal connections (people we have a
    /// chat with or in our contacts) unless My Story's list keeps some out
    /// ("All except…") or names who ("Only share with…").
    async fn my_story_recipients(&self) -> (Vec<String>, bool) {
        let lists: Vec<crate::StoryList> = self
            .db
            .setting(crate::SETTING_STORY_LISTS)
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let my_story = lists.into_iter().find(|l| l.id == crate::MY_STORY_ID);
        let me = self.self_aci();
        let connections = || async {
            let mut people: Vec<String> = self
                .db
                .people()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| p.id.as_direct().map(str::to_owned))
                .filter(|sid| sid != &me && sid != signal_store::SIGNAL_ACI)
                .collect();
            people.sort();
            people.dedup();
            people
        };
        match my_story {
            Some(list) if !list.block_list => {
                let members = list.members.into_iter().filter(|m| m != &me && m != signal_store::SIGNAL_ACI).collect();
                (members, list.allows_replies)
            }
            Some(list) => {
                let mut people = connections().await;
                people.retain(|p| !list.members.contains(p));
                (people, list.allows_replies)
            }
            None => (connections().await, true),
        }
    }

    /// Desktop's `sendStoryMessage`: the photo or video uploaded once, then
    /// the story to each target's recipients, one by one in the outbound
    /// queue, and a transcript to our other devices when done.
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
        for target in targets {
            // Desktop: lists share one timestamp; each group gets its own.
            let timestamp = match &target {
                crate::StoryTarget::MyStory => base,
                // Its own, so a later send never takes it.
                crate::StoryTarget::Group(_) => self.next_timestamp(),
            };
            let (recipients, allows_replies, group) = match &target {
                crate::StoryTarget::MyStory => {
                    let (people, replies) = self.my_story_recipients().await;
                    (people, replies, None)
                }
                crate::StoryTarget::Group(group) => (self.group_story_members(group).await, true, Some(group.clone())),
            };
            let message = presage::proto::StoryMessage {
                group: match &group {
                    Some(group) => self.group_context(group).await,
                    None => None,
                },
                attachment: Some(attachment.clone()),
                allows_replies: Some(allows_replies),
                ..Default::default()
            };
            let content = match (&story, &local) {
                (crate::OutgoingStory::Text(text), _) => signal_store::StoryContent::Text((**text).clone()),
                (crate::OutgoingStory::Media { caption, .. }, Some(meta)) => {
                    signal_store::StoryContent::Media { attachment: meta.clone(), caption: caption.clone(), styles: Vec::new() }
                }
                (crate::OutgoingStory::Media { .. }, None) => continue,
            };
            let ours = NewStory {
                author: None,
                author_name: None,
                sent_at: timestamp,
                received_at: timestamp,
                group: group.clone(),
                content,
                pointer: None,
                allows_replies,
                read: true,
            };
            if let Err(error) = self.db.insert_story(&ours, now_millis()).await {
                self.error("failed to keep the story", error);
            }
            let list_ids = if group.is_none() { vec![crate::MY_STORY_ID.to_owned()] } else { Vec::new() };
            info!(recipients = recipients.len(), group = group.is_some(), "posting a story");
            self.start_story_job(
                recipients,
                ContentBody::StoryMessage(message.clone()),
                timestamp as u64,
                StoryJobSync::Story { message: Box::new(message), list_ids, allows_replies },
                "story",
            );
        }
        self.story_expiry.notify_one();
        self.emit(Event::StoriesChanged);
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
    fn start_story_job(&self, recipients: Vec<String>, body: ContentBody, timestamp: u64, sync: StoryJobSync, what: &'static str) {
        let job = self.next_story_job.get();
        self.next_story_job.set(job + 1);
        let recipients: Vec<String> =
            recipients.into_iter().filter(|r| ServiceId::parse_from_service_id_string(r).is_some()).collect();
        self.story_jobs.borrow_mut().insert(
            job,
            StoryJob { remaining: recipients.len(), timestamp, sync, sent: Vec::new(), failures: HashMap::new(), what },
        );
        if recipients.is_empty() {
            // No one to send to: still tell our other devices.
            self.finish_story_job(job);
            return;
        }
        for recipient in recipients {
            self.enqueue(Outbound::Story { recipient, body: body.clone(), timestamp, job });
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
        let sent = match state.sync {
            StoryJobSync::Story { message, list_ids, allows_replies } => sync_message::Sent {
                timestamp: Some(state.timestamp),
                story_message: Some(*message),
                story_message_recipients: state
                    .sent
                    .iter()
                    .map(|r| sync_message::sent::StoryMessageRecipient {
                        destination_service_id: Some(r.clone()),
                        distribution_list_ids: list_ids.clone(),
                        is_allowed_to_reply: Some(allows_replies),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            StoryJobSync::Reply(data) => sync_message::Sent {
                timestamp: Some(state.timestamp),
                message: Some(*data),
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
    Story { message: Box<presage::proto::StoryMessage>, list_ids: Vec<String>, allows_replies: bool },
    Reply(Box<DataMessage>),
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
