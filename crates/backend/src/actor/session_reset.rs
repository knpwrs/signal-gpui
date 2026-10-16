//! Messages we couldn't decrypt, as Signal Desktop handles them when it
//! can't ask for the message again (`startAutomaticSessionReset` in
//! `ts/util/handleRetry.preload.ts`): the session with that device is
//! reset (`lightSessionReset` in `ts/SignalProtocolStore.preload.ts`: at
//! most hourly, archived and a null message sent to start a new one) and
//! "Chat session refreshed" goes in our chat with them. Desktop first asks
//! the sender to send the message again (a `DecryptionErrorMessage`,
//! behind `desktop.senderKey.retry`); that isn't done here, so this is
//! Desktop's fallback for when asking fails.

use presage::libsignal_service::session_store::SessionStoreExt as _;

use super::*;

/// Settings key prefix: when we last reset the session with a device
/// (unix millis), Desktop's `sessionResets` item.
const SETTING_SESSION_RESET: &str = "session_reset.";
/// Desktop resets the session with one device at most this often.
const SESSION_RESET_INTERVAL_MS: i64 = 60 * 60 * 1000;

impl Actor {
    /// We couldn't decrypt something `sender`'s `device` sent.
    pub(super) async fn on_decryption_error(self: &Rc<Self>, sender: &ServiceId, device: u32) {
        let sid = sender.service_id_string();
        let chat = ConversationId::direct(&sid);
        if sid == self.self_aci() || self.is_blocked(&chat).await {
            return;
        }
        self.light_session_reset(sender, device).await;
        // Desktop's `addChatSessionRefreshed`.
        self.ensure_title(&chat).await;
        let at = now_millis();
        for offset in 0..10 {
            match self.db.insert_notice(&chat, at + offset, &signal_store::Notice::SessionRefreshed).await {
                Ok(Some(id)) => {
                    self.emit(Event::MessageChanged { conversation: chat.clone(), message_id: id });
                    self.emit(Event::ConversationsChanged);
                    return;
                }
                Ok(None) => continue,
                Err(error) => return self.error("failed to add notice", error),
            }
        }
    }

    /// Desktop's `lightSessionReset`: at most hourly per device, the
    /// session is dropped and a null message starts a new one. A reset
    /// that couldn't be started may be tried again before the hour is up.
    async fn light_session_reset(&self, sender: &ServiceId, device: u32) {
        let key = format!("{SETTING_SESSION_RESET}{}.{device}", sender.service_id_string());
        let now = now_millis();
        let last = self.db.setting(&key).await.ok().flatten().and_then(|v| v.parse::<i64>().ok());
        if last.is_some_and(|last| reset_too_soon(last, now)) {
            info!(device, "skipping a session reset: one was done in the last hour");
            return;
        }
        let Ok(device_id) = DeviceId::new(device as u8) else { return };
        let address = ProtocolAddress::new(sender.service_id_string(), device_id);
        let store = presage::store::Store::aci_protocol_store(&self.store);
        if let Err(error) = store.delete_service_addr_device_session(&address).await {
            return warn!(%error, "couldn't reset a session");
        }
        if let Err(error) = self.db.set_setting(&key, &now.to_string()).await {
            warn!(%error, "couldn't record a session reset");
        }
        let padding: Vec<u8> = (0..rand::Rng::random_range(&mut rand::rng(), 1..=512)).map(|_| rand::random()).collect();
        let body = ContentBody::NullMessage(presage::proto::NullMessage { padding: Some(padding) });
        self.enqueue(Outbound::To { recipient: sender.service_id_string(), body, what: "null message" });
    }
}

/// Whether a reset at `last` was within the hour before `now`.
fn reset_too_soon(last: i64, now: i64) -> bool {
    now - last < SESSION_RESET_INTERVAL_MS && last <= now
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_reset_at_most_hourly() {
        assert!(reset_too_soon(1_000, 1_000 + SESSION_RESET_INTERVAL_MS - 1));
        assert!(!reset_too_soon(1_000, 1_000 + SESSION_RESET_INTERVAL_MS));
        // A reset dated in the future (the clock went back) doesn't block.
        assert!(!reset_too_soon(5_000, 1_000));
    }
}
