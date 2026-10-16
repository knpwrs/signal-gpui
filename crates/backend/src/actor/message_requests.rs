//! Message requests, as Signal Desktop answers them: the message request
//! bar's buttons (`ts/state/ducks/conversations.preload.ts`'s
//! `acceptConversation`, `blockConversation`, `deleteConversation`,
//! `reportSpam` and `blockAndReportSpam`), our other devices' answers
//! (`ts/messageModifiers/MessageRequests.dom.ts`), and what each does
//! (`applyMessageRequestResponse` in `ts/models/conversations.preload.ts`).
//! Until a chat is accepted, read receipts, typing indicators, our profile
//! key and media downloads are held (see where `is_accepted` is asked).

use signal_store::{MessageRequestResponse, Notice};

use super::*;

/// Where an answer came from (Desktop's `MessageRequestResponseSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnswerSource {
    /// Ours, here.
    Local,
    /// Another of our devices', sent at this time (unix millis).
    Sync(i64),
}

/// How many held attachments download once a request is accepted. Desktop's
/// `handleReadAndDownloadAttachments` goes through every message; here the
/// newest are fetched and older ones wait to be clicked.
const HELD_DOWNLOADS: usize = 50;

impl Actor {
    /// Whether a chat is accepted (Desktop's `isConversationAccepted`); one
    /// we have no row for has no messages, so it is.
    pub(super) async fn is_accepted(&self, conversation: &ConversationId) -> bool {
        self.db.conversation(conversation).await.ok().flatten().is_none_or(|c| c.accepted)
    }

    /// The message request bar's answer. A group invitation is taken on the
    /// group server first; only once it took is the answer kept and sent
    /// to our other devices.
    pub(super) async fn answer_message_request(self: Rc<Self>, conversation: ConversationId, answer: MessageRequestResponse) {
        if conversation.is_signal() || self.is_note_to_self(&conversation) || answer == MessageRequestResponse::Unknown {
            return;
        }
        let invited = self.db.conversation(&conversation).await.ok().flatten().is_some_and(|c| c.invited());
        // Taking an invitation is group administration's accept
        // (`group_admin.rs`), which says why when it fails.
        if answer == MessageRequestResponse::Accept && invited && !self.clone().accept_group_invite(conversation.clone()).await {
            return;
        }
        // The report names the newest messages, before anything is deleted.
        let report = if answer.spam() { self.spam_report(&conversation).await } else { None };
        self.apply_message_request(&conversation, answer, AnswerSource::Local).await;
        self.sync_message_request(&conversation, answer);
        if let Some((aci, guids, token)) = report {
            let this = self.clone();
            tokio::task::spawn_local(async move { this.send_spam_report(aci, guids, token).await });
        }
        self.emit(Event::MessageRequestAnswered { conversation, answer });
    }

    /// Another of our devices answered a request (Desktop's `onResponse`).
    pub(super) async fn apply_message_request_sync(
        self: &Rc<Self>,
        manager: &SignalManager,
        thread: mapping::ThreadRef,
        answer: MessageRequestResponse,
        at: i64,
    ) {
        let Some(conversation) = self.resolve_thread(manager, &thread).await else {
            return warn!("message request answer for a group we don't know");
        };
        if conversation.is_signal() || self.is_note_to_self(&conversation) {
            return;
        }
        self.apply_message_request(&conversation, answer, AnswerSource::Sync(at)).await;
    }

    /// Desktop's `applyMessageRequestResponse`: the answer, its notices, and
    /// what it does. Only our own answers change Storage Service, leave
    /// groups and share our profile key; the device that answered does
    /// that for one from elsewhere.
    pub(super) async fn apply_message_request(self: &Rc<Self>, conversation: &ConversationId, answer: MessageRequestResponse, source: AnswerSource) {
        let local = source == AnswerSource::Local;
        let at = match source {
            AnswerSource::Local => self.next_timestamp(),
            AnswerSource::Sync(at) => at,
        };
        // Desktop's `lookupOrCreate` for a person we have no chat with.
        self.ensure_title(conversation).await;
        let Some(before) = self.db.conversation(conversation).await.ok().flatten() else { return };
        let group = conversation.as_group().is_some();
        if let Err(error) = self.db.set_message_request(conversation, answer).await {
            return self.error("failed to save the answer", error);
        }
        // Notices a millisecond apart, as Desktop's `messageCountFromEnvelope`.
        let who = conversation.as_direct().map(str::to_owned);
        for (ix, event) in crate::message_requests::answer_events(before.message_request, before.blocked, answer).into_iter().enumerate() {
            self.notice(conversation, at + ix as i64, Notice::MessageRequest { event, group, who: who.clone() }).await;
        }
        let mut edits = Vec::new();
        if answer == MessageRequestResponse::Accept {
            if before.blocked {
                self.set_blocked_here(conversation, false).await;
                edits.push(storage::ChatEdit::Blocked(false));
            }
            self.set_profile_sharing_here(conversation, true).await;
            edits.push(storage::ChatEdit::ProfileSharing(true));
            if !before.accepted {
                self.download_held(conversation).await;
            }
            if local && !group {
                self.send_profile_key(conversation).await;
            }
        } else {
            let block = answer.blocks();
            let delete = answer.deletes();
            if block && !before.blocked {
                self.set_blocked_here(conversation, true).await;
                edits.push(storage::ChatEdit::Blocked(true));
            }
            if block || delete {
                self.set_profile_sharing_here(conversation, false).await;
                edits.push(storage::ChatEdit::ProfileSharing(false));
            }
            if answer.spam()
                && let Err(error) = self.db.set_reported(conversation).await
            {
                self.error("failed to save the report", error);
            }
            // Desktop's "Block and Leave" / "Delete and Leave", before the
            // messages go. An invitation is declined, and a declined one
            // is only deleted once the group server took it, as group
            // administration's `decline_group_invite`.
            let mut deletable = true;
            if local && group && (block || delete) && !before.left {
                if before.invited() {
                    deletable = self.decline_invite(conversation).await || block;
                } else {
                    self.clone().leave_group(conversation.clone()).await;
                }
            }
            if delete && deletable {
                self.delete_request_messages(conversation, block || before.blocked).await;
            }
        }
        self.emit(Event::ConversationsChanged);
        if local && let Some(manager) = self.manager() {
            match storage::write_chat_all(&manager, conversation, &edits).await {
                Ok(()) if !edits.is_empty() => self.storage_written(),
                Ok(()) => {}
                Err(error) => {
                    self.error("couldn't update the chat on your other devices", format!("{error:#}"));
                    self.sync_storage(&manager).await;
                }
            }
        }
    }

    async fn set_blocked_here(&self, conversation: &ConversationId, blocked: bool) {
        if let Err(error) = self.db.set_blocked(conversation, blocked).await {
            self.error("failed to update chat", error);
        }
    }

    async fn set_profile_sharing_here(&self, conversation: &ConversationId, on: bool) {
        if let Err(error) = self.db.set_profile_sharing(conversation, on).await {
            warn!(error = %format!("{error:#}"), "failed to save profile sharing");
        }
    }

    /// Desktop's `destroyMessages` from a message request: the messages go.
    /// A blocked chat stays (and with it the block); another goes too.
    async fn delete_request_messages(&self, conversation: &ConversationId, blocked: bool) {
        if !blocked {
            return self.remove_conversation(conversation).await;
        }
        match self.db.clear_messages(conversation).await {
            Ok(()) => self.emit(Event::ConversationCleared(conversation.clone())),
            Err(error) => self.error("failed to delete chat", error),
        }
    }

    /// Tells our other devices how we answered (Desktop's
    /// `getMessageRequestResponseSync`).
    fn sync_message_request(&self, conversation: &ConversationId, answer: MessageRequestResponse) {
        let binary = crate::remote_config::current().binary_service_id();
        let answer = match conversation.as_group() {
            Some(key) => {
                let Ok(key) = <[u8; 32]>::try_from(key) else { return };
                mapping::message_request_sync(None, Some(crate::groups::group_identifier(key)), answer, binary)
            }
            None => mapping::message_request_sync(conversation.as_direct(), None, answer, binary),
        };
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::MessageRequestResponse(answer)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Our profile key, to someone whose request we accepted (Desktop's
    /// `sendProfileKeyUpdate`: a data message with `PROFILE_KEY_UPDATE`).
    async fn send_profile_key(&self, conversation: &ConversationId) {
        let Some(manager) = self.manager() else { return };
        let key = self.our_profile_key(&manager).await;
        let timestamp = self.next_timestamp() as u64;
        let data = DataMessage {
            flags: Some(data_message::Flags::ProfileKeyUpdate as u32),
            profile_key: Some(key.get_bytes().to_vec()),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation: conversation.clone(), body: data.into(), timestamp, message: None });
    }

    /// Downloads what was held back while the chat was a request (Desktop's
    /// `handleReadAndDownloadAttachments`): photos, GIFs and voice messages
    /// as they'd have downloaded on their own, and long texts.
    async fn download_held(self: &Rc<Self>, conversation: &ConversationId) {
        let Some(manager) = self.manager() else { return };
        let messages = self.db.messages(conversation, None, 200).await.unwrap_or_default();
        let mut downloads = Vec::new();
        let mut long_texts = Vec::new();
        for message in messages.iter().rev().filter(|m| m.direction == Direction::Incoming && !m.deleted && m.view_once.is_none()) {
            if message.long_text.as_ref().is_some_and(|l| l.state == signal_store::LongTextState::Failed && l.attempts == 0) {
                long_texts.push(message.id);
            }
            for (index, attachment) in message.attachments.iter().enumerate() {
                if attachment.local_path.is_none() && auto_downloads(attachment) {
                    downloads.push((message.id, index));
                }
            }
        }
        downloads.truncate(HELD_DOWNLOADS);
        if downloads.is_empty() && long_texts.is_empty() {
            return;
        }
        info!(attachments = downloads.len(), long_texts = long_texts.len(), "downloading what a message request held");
        let this = self.clone();
        tokio::task::spawn_local(async move {
            for id in long_texts {
                this.download_long_text(&manager, id, LongTextTry::First).await;
            }
            for (id, index) in downloads {
                this.clone().fetch_attachment(id, index).await;
            }
        });
    }

    /// Who a spam report is about, which messages it names, and their
    /// token (Desktop's `getDirectConversationForReportSpam` and
    /// `addReportSpamJob`): in a group, whoever added us. `None` when
    /// there's nobody to report or no message to name.
    async fn spam_report(&self, conversation: &ConversationId) -> Option<(String, Vec<String>, Option<String>)> {
        let me = self.self_aci();
        let (aci, author) = match conversation.as_direct() {
            Some(aci) => (aci.to_owned(), None),
            None => {
                let added_by = self.db.conversation(conversation).await.ok().flatten().and_then(|c| c.inviter().map(str::to_owned));
                match added_by {
                    Some(aci) if aci != me => (aci.clone(), Some(aci)),
                    Some(_) => {
                        warn!("we added ourselves to this group; there's nobody to report");
                        return None;
                    }
                    None => {
                        warn!("no one to report: we don't know who added us to this group");
                        return None;
                    }
                }
            }
        };
        let guids = match self.db.server_guids_for_spam(conversation, author.as_deref()).await {
            Ok(guids) => guids,
            Err(error) => {
                self.error("couldn't read the messages to report", error);
                return None;
            }
        };
        if guids.is_empty() {
            // Desktop: "This can happen under normal conditions."
            info!("no server GUIDs to report");
            return None;
        }
        let token = self.db.reporting_token(&aci).await.ok().flatten();
        Some((aci, guids, token))
    }

    /// Sends a spam report (Desktop's `reportSpamJobQueue`), each message
    /// at once, retried as Desktop retries.
    async fn send_spam_report(self: Rc<Self>, aci: String, guids: Vec<String>, token: Option<String>) {
        let Some(manager) = self.manager() else { return };
        let reports = guids.iter().map(|guid| crate::message_requests::report(&manager, &aci, guid, token.as_deref()));
        for result in futures::future::join_all(reports).await {
            if let Err(error) = result {
                warn!(error = %format!("{error:#}"), "spam report failed");
            }
        }
    }

    /// Keeps the report spam token an envelope from them carried (Desktop's
    /// `updateReportingToken`).
    pub(super) async fn keep_reporting_token(&self, sender: &str, server_guid: Option<presage::libsignal_service::prelude::Uuid>) {
        use base64::Engine as _;
        let Some(token) = server_guid.and_then(presage::libsignal_service::cipher::take_report_spam_token) else { return };
        let token = base64::engine::general_purpose::STANDARD.encode(token);
        if let Err(error) = self.db.set_reporting_token(sender, &token).await {
            warn!(%error, "failed to keep a report spam token");
        }
    }
}

/// Whether an attachment downloads on its own (photos, GIFs and voice
/// messages under the auto-download size), as `apply` decides for one
/// arriving.
pub(super) fn auto_downloads(attachment: &AttachmentMeta) -> bool {
    let kind = attachment.content_type.as_str();
    (attachment.gif || attachment.voice || kind.starts_with("image/"))
        && attachment.size > 0
        && crate::remote_config().auto_downloads(attachment.size)
}
