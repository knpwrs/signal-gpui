//! The call history as a whole: reading it (unseen calls become seen and
//! start disappearing) and clearing it, here and from our other devices,
//! with Desktop's call log event syncs (`ts/util/callDisposition.preload.ts`:
//! `markAllCallHistoryReadAndSync`, `clearCallHistoryDataAndSync`;
//! `ts/util/onCallLogEventSync.preload.ts`).

use presage::proto::sync_message::call_log_event::Type as CallLogType;
use signal_store::{CallRef, CallTarget};

use super::*;
use crate::mapping::{CallLogEventSync, CallLogKind};

impl Actor {
    /// The chat a call event's peer id names: an ACI's 16 bytes, or a
    /// group's identifier. (A call link's room id names none.)
    async fn call_peer_conversation(&self, bytes: &[u8]) -> Option<ConversationId> {
        if let Ok(uuid) = <[u8; 16]>::try_from(bytes) {
            return Some(ConversationId::direct(&Aci::from_uuid_bytes(uuid).service_id_string()));
        }
        let manager = self.manager()?;
        let key = self.group_for_id(&manager, bytes).await?;
        Some(ConversationId::group(&key))
    }

    /// A chat as a call event's peer id (Desktop's `getBytesForPeerId`).
    fn call_peer_bytes(conversation: &ConversationId) -> Option<Vec<u8>> {
        if let Some(key) = conversation.as_group().and_then(|k| <[u8; 32]>::try_from(k).ok()) {
            return Some(crate::groups::group_identifier(key));
        }
        Some(Aci::parse_from_service_id_string(conversation.as_direct()?)?.service_id_binary())
    }

    /// The call history changed: what the timeline and chat list show.
    fn call_history_changed(&self, removed: bool, changed: Vec<(ConversationId, i64)>) {
        for (conversation, message_id) in changed {
            if removed {
                self.emit(Event::MessageRemoved { conversation, message_id });
            } else {
                self.emit(Event::MessageChanged { conversation, message_id });
            }
        }
        self.emit(Event::ConversationsChanged);
        self.expiry.notify_one();
    }

    /// Marks calls read up to the latest (in a chat: its latest unseen
    /// one), and tells our other devices. Desktop does this when the Calls
    /// tab is viewed (`markCallsTabViewed`) and when a chat with an unseen
    /// call is read (`markCallHistoryReadInConversation`).
    pub(super) async fn mark_calls_read(&self, conversation: Option<ConversationId>) {
        let call = match &conversation {
            Some(conversation) => self.db.latest_unseen_call(conversation).await,
            None => self.db.latest_call(None).await,
        };
        let call = match call {
            Ok(Some(call)) => call,
            Ok(None) => return,
            Err(error) => return self.error("failed to read the call history", error),
        };
        let target = CallTarget { conversation: Some(call.conversation.clone()), call_id: call.call_id, timestamp: call.sent_at };
        let active = self.active_group_call_id();
        let active = active.as_ref().map(|(c, id)| (c, *id));
        match self.db.mark_calls_seen(&target, conversation.is_some(), now_millis(), active).await {
            Ok(changed) => self.call_history_changed(false, changed),
            Err(error) => return self.error("failed to mark calls read", error),
        }
        let kind = if conversation.is_some() { CallLogType::MarkedAsReadInConversation } else { CallLogType::MarkedAsRead };
        self.sync_call_log(kind, &call);
    }

    /// Clears the call history up to its latest call, and tells our other
    /// devices (Desktop's "Clear call history").
    pub(super) async fn clear_call_history(&self) {
        let call = match self.db.latest_call(None).await {
            Ok(Some(call)) => call,
            Ok(None) => return,
            Err(error) => return self.error("failed to read the call history", error),
        };
        let target = CallTarget { conversation: Some(call.conversation.clone()), call_id: call.call_id, timestamp: call.sent_at };
        match self.db.clear_call_history(&target).await {
            Ok(removed) => self.call_history_changed(true, removed),
            Err(error) => return self.error("failed to clear the call history", error),
        }
        self.sync_call_log(CallLogType::Clear, &call);
    }

    /// Desktop's `CallLogEvent` sync, naming the call it goes up to.
    fn sync_call_log(&self, kind: CallLogType, call: &CallRef) {
        let event = presage::proto::sync_message::CallLogEvent {
            r#type: Some(kind.into()),
            timestamp: Some(call.sent_at as u64),
            conversation_id: Self::call_peer_bytes(&call.conversation),
            call_id: call.call_id.map(|id| id as u64),
        };
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::CallLogEvent(event)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Another of our devices read or cleared its call history.
    pub(super) async fn on_call_log_event(&self, event: CallLogEventSync) {
        let conversation = match &event.conversation {
            Some(bytes) => self.call_peer_conversation(bytes).await,
            None => None,
        };
        // A call we can't place names nothing (Desktop's
        // `getCallHistoryForCallLogEventTarget` finds none).
        if event.conversation.is_some() && conversation.is_none() {
            return;
        }
        let target = CallTarget { conversation, call_id: event.call_id, timestamp: event.timestamp };
        let result = match event.kind {
            CallLogKind::Clear => self.db.clear_call_history(&target).await.map(|removed| (true, removed)),
            CallLogKind::MarkedAsRead | CallLogKind::MarkedAsReadInConversation => {
                let in_conversation = event.kind == CallLogKind::MarkedAsReadInConversation;
                let active = self.active_group_call_id();
                let active = active.as_ref().map(|(c, id)| (c, *id));
                let read_at = event.at.min(now_millis());
                self.db.mark_calls_seen(&target, in_conversation, read_at, active).await.map(|changed| (false, changed))
            }
        };
        match result {
            Ok((removed, changed)) => self.call_history_changed(removed, changed),
            Err(error) => self.error("failed to update the call history", error),
        }
    }
}
