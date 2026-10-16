//! View-once messages, as Desktop handles them: opening one once
//! (`ts/state/ducks/lightbox.preload.ts`'s `showLightboxForViewOnceMedia`,
//! `ts/services/MessageUpdater.preload.ts`'s `markViewOnceMessageViewed`),
//! our other devices' opens (`ts/messageModifiers/ViewOnceOpenSyncs.preload.ts`,
//! early ones kept in `early.rs`), and erasing the ones left unviewed
//! (`ts/services/tapToViewMessagesDeletionService.preload.ts`).

use signal_store::ViewOnce;

use super::*;

/// Where an opened view-once message's media is copied to be shown: it's
/// deleted when the viewer closes, and anything left is cleared at start.
pub(crate) const VIEW_ONCE_DIR: &str = "view-once";

impl Actor {
    /// Erases a view-once message that's still ready, deleting its files.
    /// `true` if it did.
    pub(super) async fn erase_view_once(&self, message_id: i64, to: ViewOnce) -> bool {
        self.try_erase_view_once(message_id, to).await.unwrap_or(false)
    }

    /// [`erase_view_once`](Self::erase_view_once), with `Err` when the
    /// database failed (and said so).
    async fn try_erase_view_once(&self, message_id: i64, to: ViewOnce) -> Result<bool, ()> {
        match self.db.erase_view_once(message_id, to).await {
            Ok(Some(erased)) => {
                for file in &erased.files {
                    let _ = tokio::fs::remove_file(file).await;
                }
                if to == ViewOnce::Viewed {
                    // Read now: a disappearing one starts counting down.
                    self.expiry.notify_one();
                }
                self.emit(Event::MessageChanged { conversation: erased.conversation, message_id });
                self.emit(Event::ConversationsChanged);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => {
                self.error("failed to erase a view-once message", error);
                Err(())
            }
        }
    }

    /// Desktop's `showLightboxForViewOnceMedia`: copies the media to a
    /// temporary file, erases the message (viewed), and hands the copy to
    /// the viewer; then tells the sender (with read receipts on) and our
    /// other devices.
    pub(super) async fn open_view_once(&self, message_id: i64) {
        let Ok(Some(message)) = self.db.message(message_id).await else { return };
        let (Some(ViewOnce::Ready), Direction::Incoming, Some(author)) =
            (message.view_once, message.direction, message.author.clone())
        else {
            return;
        };
        let Some(attachment) = message.attachments.first().cloned() else { return };
        let Some(local) = attachment.local_path.clone() else {
            return self.error("can't open view-once media", "it hasn't downloaded yet");
        };
        let dir = self.config.data_dir.join(VIEW_ONCE_DIR);
        let ext = std::path::Path::new(&local).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let copy = dir.join(format!("{:016x}.{ext}", rand::random::<u64>()));
        let copied = async {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::copy(&local, &copy).await
        };
        if let Err(error) = copied.await {
            return self.error("can't open view-once media", error);
        }
        if !self.erase_view_once(message_id, ViewOnce::Viewed).await {
            // Viewed (or gone) meanwhile, here or on another device.
            let _ = tokio::fs::remove_file(&copy).await;
            return;
        }
        self.emit(Event::ViewOnceOpened(crate::OpenedViewOnce {
            message_id,
            path: copy,
            content_type: attachment.content_type.clone(),
            size: attachment.width.zip(attachment.height),
        }));
        // A viewed receipt (`ReceiptType.Viewed`), which read receipts
        // govern (`sendReceipts`).
        if self.setting_on(crate::AccountSetting::ReadReceipts).await {
            let receipt = ReceiptMessage {
                r#type: Some(receipt_message::Type::Viewed.into()),
                timestamp: vec![message.sent_at as u64],
            };
            self.enqueue(Outbound::Send {
                conversation: ConversationId::direct(&author),
                body: receipt.into(),
                timestamp: self.next_timestamp() as u64,
                message: None,
            });
        }
        // `syncViewOnceOpen`, the sender named as the remote config says.
        let sender = crate::remote_config::current().wire_service_id(&author);
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::ViewOnceOpen(sync_message::ViewOnceOpen {
                    sender_aci: sender.string(),
                    timestamp: Some(message.sent_at as u64),
                    sender_aci_binary: sender.binary(),
                })),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// We opened their view-once message on another device: erased here
    /// too (Desktop's `ViewOnceOpenSyncs.onSync`). One not here yet is
    /// kept as an early event (`early.rs`), as Desktop keeps it as a sync
    /// task, and applied when it comes.
    pub(super) async fn apply_view_once_open(&self, author: &str, sent_at: i64) -> early::Outcome {
        use early::Outcome;
        match self.db.find_message_any(Some(author), sent_at).await {
            Ok(Some(message)) if message.view_once == Some(ViewOnce::Ready) => {
                match self.try_erase_view_once(message.id, ViewOnce::Viewed).await {
                    Ok(true) => Outcome::Applied,
                    Ok(false) => Outcome::Refused,
                    Err(()) => Outcome::Failed,
                }
            }
            // Viewed already, or not view-once.
            Ok(Some(_)) => Outcome::Refused,
            Ok(None) => Outcome::Missing,
            Err(error) => {
                self.error("failed to apply a view-once open", error);
                Outcome::Failed
            }
        }
    }

    /// Desktop's tap-to-view deletion service: a view-once message not yet
    /// erased (theirs unviewed, or ours from before) goes once it's older
    /// than the server keeps messages. Woken when one arrives.
    pub(super) async fn view_once_expiry_loop(self: Rc<Self>) {
        loop {
            let max_age = crate::remote_config().message_queue_time_ms();
            match self.db.view_once_to_erase(now_millis().saturating_sub(max_age)).await {
                Ok(ids) => {
                    for id in ids {
                        self.erase_view_once(id, ViewOnce::Gone).await;
                    }
                }
                Err(error) => warn!(%error, "couldn't look for view-once messages to erase"),
            }
            // At least a second between runs, at most a day.
            let wait = match self.db.oldest_view_once().await.ok().flatten() {
                Some(at) => at.saturating_add(max_age).saturating_sub(now_millis()).clamp(1_000, 24 * 60 * 60 * 1000),
                None => 24 * 60 * 60 * 1000,
            };
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(wait as u64)) => {}
                () = self.view_once_expiry.notified() => {}
            }
        }
    }

    /// Copies left by a viewer that didn't close (the app quit).
    pub(super) async fn clear_view_once_copies(&self) {
        let _ = tokio::fs::remove_dir_all(self.config.data_dir.join(VIEW_ONCE_DIR)).await;
    }
}
