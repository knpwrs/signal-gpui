//! Events that change a message, and what to do when they come before it.
//!
//! Desktop keeps a delete for everyone, an edit, a reaction, a poll vote or
//! end, a receipt, a read sync, a view-once open or a "delete for me" sync
//! whose message isn't here yet, and applies it when the message comes
//! (`ts/util/modifyTargetMessage.preload.ts`, with `Deletes.forMessage`,
//! `Edits.forMessage`, `Reactions.findReactionsForMessage` and the rest in
//! `ts/messageModifiers/`). It keeps them in memory and keeps what they came
//! in on disk until they're applied, so a restart reads them again; the
//! store's `early.rs` has how long. Here they're kept in the store only,
//! looked up when a message (or an edit, whose timestamp names it too)
//! arrives, and all tried again at each start and after a history
//! transfer. Pins aren't kept: Desktop drops a pin for a message it
//! doesn't have.

use signal_store::EarlyKeep;

use super::*;

/// What came of applying an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Applied,
    /// Its message isn't here (yet), or (a vote in our poll) hasn't
    /// reached its voter yet.
    Missing,
    /// It doesn't apply: too late, not allowed, or already done.
    Refused,
    /// The database failed; kept events stay to be tried again.
    Failed,
}

/// What the events kept for a new message did to it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Early {
    /// Read on another device first.
    pub read: bool,
    /// Deleted for everyone first.
    pub deleted: bool,
}

/// The message an event is for, `(author or None for us, sent at)`, and
/// how Desktop keeps it. Events for several messages are kept one by one.
fn target(action: &Action) -> Option<(Option<&str>, i64, EarlyKeep)> {
    use EarlyKeep::{Envelope, SyncTask};
    Some(match action {
        Action::React { target_author, target_sent_at, .. }
        | Action::Delete { target_author, target_sent_at, .. }
        | Action::AdminDelete { target_author, target_sent_at, .. }
        | Action::Edit { target_author, target_sent_at, .. }
        | Action::PollVote { target_author, target_sent_at, .. } => (target_author.as_deref(), *target_sent_at, Envelope),
        Action::PollEnd { by, target_sent_at, .. } => (by.as_deref(), *target_sent_at, Envelope),
        Action::Receipt { sent_at, .. } if sent_at.len() == 1 => (None, sent_at[0], SyncTask),
        Action::ReadSync(entries) if entries.len() == 1 => (Some(entries[0].0.as_str()), entries[0].1, SyncTask),
        // Desktop's `ViewOnceOpenSyncs` is a sync task too.
        Action::ViewOnceOpen { author, sent_at } => (Some(author.as_str()), *sent_at, SyncTask),
        Action::DeleteForMe { messages, conversations } if messages.len() == 1 && conversations.is_empty() => {
            (messages[0].1.as_deref(), messages[0].2, SyncTask)
        }
        _ => return None,
    })
}

impl Actor {
    /// Applies an event that changes a message, keeping it for later if
    /// the message isn't here yet.
    pub(super) async fn modify(self: &Rc<Self>, manager: &SignalManager, action: Action) -> Outcome {
        let outcome = self.modify_once(manager, &action).await;
        if outcome == Outcome::Missing {
            self.keep_early(&action).await;
        }
        outcome
    }

    async fn keep_early(&self, action: &Action) {
        let Some((author, sent_at, keep)) = target(action) else { return };
        let json = match serde_json::to_string(action) {
            Ok(json) => json,
            Err(error) => return warn!(%error, "couldn't keep an early event"),
        };
        debug!(sent_at, "keeping an event until its message comes");
        if let Err(error) = self.db.keep_early_event(author, sent_at, keep, &json, now_millis()).await {
            self.error("failed to keep an early event", error);
        }
    }

    async fn modify_once(self: &Rc<Self>, manager: &SignalManager, action: &Action) -> Outcome {
        match action {
            Action::React { .. } => self.apply_reaction(action).await,
            Action::Delete { .. } => self.apply_delete(action).await,
            Action::AdminDelete { .. } => self.apply_admin_delete(action).await,
            Action::Edit { .. } => self.apply_edit(manager, action).await,
            Action::PollVote { .. } => self.apply_poll_vote(action).await,
            Action::PollEnd { .. } => self.apply_poll_end(manager, action).await,
            Action::Receipt { sent_at, status, sender } if sent_at.len() == 1 => {
                self.apply_receipt(sent_at[0], *status, sender.as_deref()).await
            }
            Action::ReadSync(entries) if entries.len() == 1 => self.apply_read_sync(&entries[0].0, entries[0].1).await,
            Action::ViewOnceOpen { author, sent_at } => self.apply_view_once_open(author, *sent_at).await,
            Action::DeleteForMe { messages, conversations } if messages.len() == 1 && conversations.is_empty() => {
                let (thread, author, sent_at) = &messages[0];
                self.apply_delete_for_me(manager, thread, author.as_deref(), *sent_at).await
            }
            _ => Outcome::Refused,
        }
    }

    /// The message by `author` (`None`: us) sent at `sent_at` is here, or
    /// has an edit sent then: applies what came for it first, oldest first.
    pub(super) async fn apply_early_for(self: &Rc<Self>, manager: &SignalManager, author: Option<&str>, sent_at: i64) -> Early {
        match self.db.early_events_for(author, sent_at).await {
            Ok(events) => self.replay(manager, events).await,
            Err(error) => {
                self.error("failed to look up early events", error);
                Early::default()
            }
        }
    }

    /// Tries every kept event again: at startup (`startup`, which counts
    /// an attempt and drops what's been kept long enough, as Desktop does
    /// when it reads its caches again) and after a history transfer.
    pub(super) async fn retry_early_events(self: &Rc<Self>, manager: &SignalManager, startup: bool) {
        let events = if startup { self.db.load_early_events(now_millis()).await } else { self.db.early_events().await };
        match events {
            Ok(events) if !events.is_empty() => {
                info!(count = events.len(), "trying early events again");
                self.replay(manager, events).await;
            }
            Ok(_) => {}
            Err(error) => self.error("failed to load early events", error),
        }
    }

    async fn replay(self: &Rc<Self>, manager: &SignalManager, events: Vec<signal_store::EarlyEvent>) -> Early {
        let mut early = Early::default();
        let mut changed = false;
        for event in events {
            // The history transfer's retry and a new message can reach the
            // same event at once; one of them applies it.
            if !self.early_replaying.borrow_mut().insert(event.id) {
                continue;
            }
            let outcome = match serde_json::from_str::<Action>(&event.action) {
                Ok(action) => {
                    let outcome = self.modify_once(manager, &action).await;
                    if outcome == Outcome::Applied {
                        debug!("applied an event that came before its message");
                        changed = true;
                        match action {
                            // Viewing one reads it too (`markViewOnceMessageViewed`).
                            Action::ReadSync(_) | Action::ViewOnceOpen { .. } => early.read = true,
                            Action::Delete { .. } | Action::AdminDelete { .. } => early.deleted = true,
                            _ => {}
                        }
                    }
                    outcome
                }
                Err(error) => {
                    warn!(%error, "dropping an early event that can't be read");
                    Outcome::Refused
                }
            };
            // Only once it's done with: a database failure leaves it to
            // be tried again.
            if matches!(outcome, Outcome::Applied | Outcome::Refused)
                && let Err(error) = self.db.remove_early_event(event.id).await
            {
                self.error("failed to remove an early event", error);
            }
            self.early_replaying.borrow_mut().remove(&event.id);
        }
        if changed {
            self.emit(Event::ConversationsChanged);
        }
        early
    }

    async fn apply_reaction(&self, action: &Action) -> Outcome {
        let Action::React { conversation, target_author, target_sent_at, emoji, author, sent_at, remove } = action else {
            return Outcome::Refused;
        };
        let (target, ours) = match self.db.find_message(conversation, target_author.as_deref(), *target_sent_at).await {
            Ok(Some(message)) => (message.id, message.direction == Direction::Outgoing),
            Ok(None) => {
                debug!("reaction target not found");
                return Outcome::Missing;
            }
            Err(error) => {
                self.error("failed to look up message", error);
                return Outcome::Failed;
            }
        };
        let result = if *remove {
            self.db.remove_reaction(target, author.as_deref()).await
        } else {
            self.db
                .set_reaction(target, Reaction { emoji: emoji.clone(), author: author.clone(), sent_at: *sent_at })
                .await
        };
        match result {
            Ok(()) => {
                self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: target });
                // Desktop's `doesReactionWarrantNotification`: someone
                // else's reaction to one of ours.
                if ours && let Some(author) = author {
                    let sender = self.name_of_plain(author).await.unwrap_or_else(|| short_id(author));
                    self.emit(Event::Reacted {
                        conversation: conversation.clone(),
                        message_id: target,
                        author: author.clone(),
                        sender,
                        emoji: (!remove).then(|| emoji.clone()),
                    });
                }
                Outcome::Applied
            }
            Err(error) => {
                self.error("failed to store reaction", error);
                Outcome::Failed
            }
        }
    }

    /// The author's delete for everyone. Found by author too, so only the
    /// author's delete matches. A story of theirs goes the same way.
    async fn apply_delete(&self, action: &Action) -> Outcome {
        let Action::Delete { conversation, target_author, target_sent_at, sent_at } = action else {
            return Outcome::Refused;
        };
        let message = match self.db.find_message(conversation, target_author.as_deref(), *target_sent_at).await {
            Ok(Some(message)) => message,
            Ok(None) if self.apply_story_delete(target_author.as_deref(), *target_sent_at).await => return Outcome::Applied,
            // Or a reply to a group story.
            Ok(None) if self.apply_story_reply_delete(conversation, target_author.as_deref(), *target_sent_at).await => return Outcome::Applied,
            Ok(None) => {
                info!("keeping a delete for everyone until its message comes");
                return Outcome::Missing;
            }
            Err(error) => {
                self.error("failed to look up message", error);
                return Outcome::Failed;
            }
        };
        if !crate::deletes::can_receive(&message, self.is_note_to_self(conversation), *sent_at) {
            info!("ignoring a delete for everyone that came too late");
            return Outcome::Refused;
        }
        // A view-once message's media goes too.
        self.erase_view_once(message.id, signal_store::ViewOnce::Gone).await;
        match self.db.mark_deleted(message.id).await {
            Ok(()) => {
                self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: message.id });
                self.emit(Event::ConversationsChanged);
                Outcome::Applied
            }
            Err(error) => {
                self.error("failed to delete message", error);
                Outcome::Failed
            }
        }
    }

    async fn apply_admin_delete(&self, action: &Action) -> Outcome {
        let Action::AdminDelete { conversation, target_author, target_sent_at, by, sent_at } = action else {
            return Outcome::Refused;
        };
        // Desktop's `isAdminDeleteReceiveEnabled` (`ts/background.preload.ts`).
        if !crate::remote_config().admin_delete_receive() {
            info!("ignoring an admin delete: not enabled");
            return Outcome::Refused;
        }
        let message = match self.db.find_message(conversation, target_author.as_deref(), *target_sent_at).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                info!("keeping an admin delete until its message comes");
                return Outcome::Missing;
            }
            Err(error) => {
                self.error("failed to look up message", error);
                return Outcome::Failed;
            }
        };
        // Their role when it's applied (presage has fetched the group
        // again if the message named a newer revision).
        let deleter = by.clone().unwrap_or_else(|| self.self_aci());
        let is_admin = self.is_group_admin(conversation, &deleter).await;
        let group = conversation.as_group().is_some();
        if let Err(reason) = crate::deletes::can_receive_as_admin(&message, group, is_admin, *sent_at) {
            info!(reason, "ignoring an admin delete");
            return Outcome::Refused;
        }
        self.erase_view_once(message.id, signal_store::ViewOnce::Gone).await;
        match self.db.mark_deleted_by_admin(message.id, &signal_store::DeletedByAdmin { by: by.clone() }).await {
            Ok(true) => {
                self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: message.id });
                self.emit(Event::ConversationsChanged);
                Outcome::Applied
            }
            Ok(false) => Outcome::Refused,
            Err(error) => {
                self.error("failed to delete message", error);
                Outcome::Failed
            }
        }
    }

    async fn apply_edit(self: &Rc<Self>, manager: &SignalManager, action: &Action) -> Outcome {
        let Action::Edit { conversation, target_author, target_sent_at, sent_at, body, mentions, styles, long_text } = action else {
            return Outcome::Refused;
        };
        let target = match self.db.find_message(conversation, target_author.as_deref(), *target_sent_at).await {
            // View-once messages can't be edited (Desktop's `handleEditMessage`).
            Ok(Some(message)) if message.view_once.is_some() => {
                info!("ignoring an edit to a view-once message");
                return Outcome::Refused;
            }
            Ok(Some(message)) => message.id,
            Ok(None) => {
                debug!("keeping an edit until its message comes");
                return Outcome::Missing;
            }
            Err(error) => {
                self.error("failed to look up message", error);
                return Outcome::Failed;
            }
        };
        let (body, spans, style_spans) = self.render_body(body, mentions, styles).await;
        match self.db.edit_message(target, &body, *sent_at).await {
            Ok(()) => {
                let _ = self.db.set_mentions(target, &spans).await;
                let _ = self.db.set_styles(target, &style_spans).await;
                if let Some(pointer) = long_text {
                    // Held in a message request, as the message's own was.
                    let held = target_author.is_some() && !self.is_accepted(conversation).await;
                    self.expect_long_text(manager, target, pointer, mentions.clone(), styles.clone(), held).await;
                }
                // What names the edit can come before it: a read sync, a
                // reaction, a delete.
                Box::pin(self.apply_early_for(manager, target_author.as_deref(), *sent_at)).await;
                self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: target });
                self.emit(Event::ConversationsChanged);
                Outcome::Applied
            }
            Err(error) => {
                self.error("failed to edit message", error);
                Outcome::Failed
            }
        }
    }

    /// A receipt for one of ours (Desktop's `MessageReceipts`): `sender`'s
    /// send state moves on (one kept from before senders were has none, and
    /// moves the message's one status).
    async fn apply_receipt(&self, sent_at: i64, status: signal_store::DeliveryStatus, sender: Option<&str>) -> Outcome {
        use signal_store::{DeliveryStatus, SendStatus};
        let applied = match sender {
            Some(sender) => {
                let status = match status {
                    DeliveryStatus::Read => SendStatus::Read,
                    DeliveryStatus::Viewed => SendStatus::Viewed,
                    _ => SendStatus::Delivered,
                };
                self.db.apply_receipt_from(sender, sent_at, status, now_millis()).await
            }
            None => self.db.update_status_by_sent_at(sent_at, status).await,
        };
        match applied {
            Ok(changed) if !changed.is_empty() => {
                for (conversation, message_id) in changed {
                    self.emit(Event::MessageChanged { conversation, message_id });
                }
                Outcome::Applied
            }
            // Nothing changed: already that far along, or not here yet.
            Ok(_) => match self.db.find_message_any(None, sent_at).await {
                Ok(Some(_)) => Outcome::Refused,
                Ok(None) => Outcome::Missing,
                Err(error) => {
                    self.error("failed to look up message", error);
                    Outcome::Failed
                }
            },
            Err(error) => {
                self.error("failed to apply receipt", error);
                Outcome::Failed
            }
        }
    }

    /// We read their message on another device; this marks it and
    /// everything older in its chat read (the caller says the chats changed).
    async fn apply_read_sync(&self, author: &str, sent_at: i64) -> Outcome {
        match self.db.apply_read_sync(author, sent_at).await {
            Ok(signal_store::ReadSync::Read(_)) => {
                self.expiry.notify_one();
                Outcome::Applied
            }
            Ok(signal_store::ReadSync::AlreadyRead) => Outcome::Refused,
            Ok(signal_store::ReadSync::NotFound) => Outcome::Missing,
            Err(error) => {
                self.error("failed to apply read sync", error);
                Outcome::Failed
            }
        }
    }

    /// "Delete for me" on another of our devices. A chat we can't find
    /// drops it, as Desktop's `DeletesForMe.onDelete` does.
    async fn apply_delete_for_me(
        &self,
        manager: &SignalManager,
        thread: &mapping::ThreadRef,
        author: Option<&str>,
        sent_at: i64,
    ) -> Outcome {
        let Some(conversation) = self.resolve_thread(manager, thread).await else { return Outcome::Refused };
        match self.db.find_message(&conversation, author, sent_at).await {
            Ok(Some(message)) => {
                self.remove_message(&conversation, message.id).await;
                Outcome::Applied
            }
            Ok(None) => Outcome::Missing,
            Err(error) => {
                self.error("failed to look up message", error);
                Outcome::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept() -> Vec<Action> {
        let chat = ConversationId::direct("bob");
        vec![
            Action::React {
                conversation: chat.clone(),
                target_author: Some("bob".into()),
                target_sent_at: 10,
                emoji: "👍".into(),
                author: None,
                sent_at: 11,
                remove: false,
            },
            Action::Delete { conversation: chat.clone(), target_author: Some("bob".into()), target_sent_at: 10, sent_at: 12 },
            Action::AdminDelete {
                conversation: ConversationId::group(&[7; 32]),
                target_author: None,
                target_sent_at: 10,
                by: Some("ann".into()),
                sent_at: 12,
            },
            Action::Edit {
                conversation: chat.clone(),
                target_author: Some("bob".into()),
                target_sent_at: 10,
                sent_at: 13,
                body: "fixed \u{FFFC}".into(),
                mentions: vec!["ann".into()],
                styles: vec![signal_store::WireStyle { start: 0, len: 5, style: signal_store::TextStyle::Bold }],
                long_text: Some(AttachmentPointer {
                    content_type: Some("text/x-signal-plain".into()),
                    size: Some(4000),
                    key: Some(vec![1, 2, 3]),
                    ..Default::default()
                }),
            },
            Action::PollVote {
                conversation: chat.clone(),
                voter: Some("bob".into()),
                target_author: None,
                target_sent_at: 10,
                options: vec![0, 2],
                vote_count: 3,
                sent_at: 14,
            },
            Action::PollEnd { conversation: chat, by: Some("bob".into()), target_sent_at: 10, sent_at: 15 },
            Action::Receipt { sent_at: vec![10], status: signal_store::DeliveryStatus::Read, sender: Some("bob".into()) },
            Action::ReadSync(vec![("bob".into(), 10)]),
            Action::ViewOnceOpen { author: "bob".into(), sent_at: 10 },
            Action::DeleteForMe {
                messages: vec![(mapping::ThreadRef::GroupId(vec![9; 32]), Some("bob".into()), 10)],
                conversations: Vec::new(),
            },
        ]
    }

    #[test]
    fn kept_events_survive_a_restart_unchanged() {
        for action in kept() {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action, "{json}");
        }
    }

    #[test]
    fn each_is_kept_for_its_message_as_desktop_keeps_it() {
        use EarlyKeep::{Envelope, SyncTask};
        let targets: Vec<_> =
            kept().iter().map(|a| target(a).map(|(author, at, keep)| (author.map(str::to_owned), at, keep))).collect();
        let bob = || Some("bob".to_owned());
        assert_eq!(
            targets,
            [
                Some((bob(), 10, Envelope)),
                Some((bob(), 10, Envelope)),
                Some((None, 10, Envelope)),
                Some((bob(), 10, Envelope)),
                Some((None, 10, Envelope)),
                // An end names its poll by its author, who ends it.
                Some((bob(), 10, Envelope)),
                Some((None, 10, SyncTask)),
                Some((bob(), 10, SyncTask)),
                // A view-once open, as Desktop's `ViewOnceOpenSyncs`.
                Some((bob(), 10, SyncTask)),
                Some((bob(), 10, SyncTask)),
            ]
        );
        // Several messages at once are kept one by one, and nothing else is.
        assert_eq!(target(&Action::ReadSync(vec![("a".into(), 1), ("b".into(), 2)])), None);
        assert_eq!(target(&Action::Receipt { sent_at: vec![1, 2], status: signal_store::DeliveryStatus::Read, sender: None }), None);
        // One kept before receipts named their sender still reads.
        let old: Action = serde_json::from_str(r#"{"Receipt":{"sent_at":[10],"status":"Read"}}"#).unwrap();
        assert_eq!(old, Action::Receipt { sent_at: vec![10], status: signal_store::DeliveryStatus::Read, sender: None });
        assert_eq!(target(&Action::StorageChanged), None);
        assert!(serde_json::to_string(&Action::StorageChanged).is_err(), "never stored");
    }
}
