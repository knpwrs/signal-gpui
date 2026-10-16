//! Delivery receipts, as Signal Desktop sends them: for messages and edits
//! that came by sealed sender, in chats we've accepted
//! (`handleDataMessage`, `handleEditMessage`), held while the server's
//! queue drains and then batched (`deliveryReceiptQueue`,
//! `deliveryReceiptBatcher` in `ts/util/deliveryReceipt.preload.ts`: half a
//! second, at most 100), one receipt per chat and sender with up to 100
//! timestamps (`sendReceipts` in `ts/util/sendReceipts.preload.ts`).
//! Delivery receipts don't depend on the read receipts setting.

use super::*;

/// Desktop's batcher waits this long for more.
const BATCH_WAIT: Duration = Duration::from_millis(500);
/// And sends at once with this many.
const BATCH_MAX: usize = 100;
/// Timestamps in one receipt (`sendReceipts`' `CHUNK_SIZE`).
const RECEIPT_CHUNK: usize = 100;

/// A message to send a delivery receipt for: its chat, sender and time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingReceipt {
    pub chat: ConversationId,
    pub sender: String,
    pub sent_at: i64,
}

/// The receipts a batch sends: per chat and sender, in arrival order,
/// timestamps in chunks.
pub(super) fn receipt_batches(pending: &[PendingReceipt]) -> Vec<(String, Vec<u64>)> {
    let mut groups: Vec<((ConversationId, String), Vec<u64>)> = Vec::new();
    for receipt in pending {
        let key = (receipt.chat.clone(), receipt.sender.clone());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, times)) => times.push(receipt.sent_at as u64),
            None => groups.push((key, vec![receipt.sent_at as u64])),
        }
    }
    groups
        .into_iter()
        .flat_map(|((_, sender), times)| times.chunks(RECEIPT_CHUNK).map(|c| (sender.clone(), c.to_vec())).collect::<Vec<_>>())
        .collect()
}

impl Actor {
    /// A message or edit `sender` sent us at `sent_at` was stored: its
    /// delivery receipt waits for the batch, if one goes.
    pub(super) async fn queue_delivery_receipt(self: &Rc<Self>, chat: ConversationId, sender: String, sent_at: i64) {
        if chat.is_signal() || !self.is_accepted(&chat).await {
            return;
        }
        self.delivery_receipts.borrow_mut().push(PendingReceipt { chat, sender, sent_at });
        self.schedule_receipts();
    }

    /// Sends now with a full batch, else in half a second, once the queue
    /// on the server has drained.
    pub(super) fn schedule_receipts(self: &Rc<Self>) {
        if !self.receipts_ready.get() {
            return;
        }
        if self.delivery_receipts.borrow().len() >= BATCH_MAX {
            return self.flush_receipts();
        }
        if self.receipts_scheduled.replace(true) {
            return;
        }
        let this = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(BATCH_WAIT).await;
            this.receipts_scheduled.set(false);
            this.flush_receipts();
        });
    }

    /// Desktop's `markViewed` for a voice message played: viewed here once,
    /// then a viewed receipt to its author (with read receipts on, as
    /// `sendReceipts` requires, and in an accepted chat) and a view sync to
    /// our other devices (ours named by our own ACI). Desktop does this for
    /// voice notes, not other audio or videos.
    pub(super) async fn mark_viewed(self: &Rc<Self>, message_id: i64) {
        let Ok(Some(message)) = self.db.message(message_id).await else { return };
        if message.conversation_id.is_signal() || !message.attachments.iter().any(|a| a.voice) {
            return;
        }
        match self.db.mark_viewed(message_id).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => return self.error("failed to mark a message viewed", error),
        }
        let author = message.author.clone().unwrap_or_else(|| self.self_aci());
        if message.direction == Direction::Incoming
            && self.setting_on(crate::AccountSetting::ReadReceipts).await
            && self.is_accepted(&message.conversation_id).await
        {
            let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Viewed.into()), timestamp: vec![message.sent_at as u64] };
            self.enqueue(Outbound::To { recipient: author.clone(), body: receipt.into(), what: "viewed receipt" });
        }
        let mut sync = SyncMessage::with_padding(&mut rand::rng());
        let sender = crate::remote_config::current().wire_service_id(&author);
        sync.viewed = vec![sync_message::Viewed {
            sender_aci: sender.string(),
            sender_aci_binary: sender.binary(),
            timestamp: Some(message.sent_at as u64),
        }];
        self.enqueue(Outbound::Sync { body: sync });
    }

    fn flush_receipts(&self) {
        let pending = std::mem::take(&mut *self.delivery_receipts.borrow_mut());
        for (sender, timestamps) in receipt_batches(&pending) {
            let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Delivery.into()), timestamp: timestamps };
            self.enqueue(Outbound::To { recipient: sender, body: receipt.into(), what: "delivery receipt" });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_go_per_chat_and_sender_in_chunks() {
        let chat = |c: &str| ConversationId(c.into());
        let mut pending = vec![
            PendingReceipt { chat: chat("d:a"), sender: "a".into(), sent_at: 1 },
            PendingReceipt { chat: chat("g:1"), sender: "a".into(), sent_at: 2 },
            PendingReceipt { chat: chat("d:a"), sender: "a".into(), sent_at: 3 },
        ];
        assert_eq!(receipt_batches(&pending), [("a".to_owned(), vec![1, 3]), ("a".to_owned(), vec![2])]);
        pending = (0..150).map(|t| PendingReceipt { chat: chat("g:1"), sender: "b".into(), sent_at: t }).collect();
        let batches = receipt_batches(&pending);
        assert_eq!(batches.iter().map(|(_, t)| t.len()).collect::<Vec<_>>(), [100, 50]);
    }
}
