//! Account settings with more to them than a flag in the account record:
//! the default disappearing timer, and who can see and find us by our
//! phone number (Desktop's `ts/state/smart/Preferences.preload.tsx`).

use presage::libsignal_service::{configuration::Endpoint, push_service::HttpAuthOverride};

use super::*;

impl Actor {
    /// Desktop's "Default timer for new chats": here, then the account
    /// record.
    pub(super) async fn set_universal_expire_timer(self: Rc<Self>, seconds: u32) {
        let settings = crate::AccountSettings { universal_expire_timer: Some(seconds), ..Default::default() };
        self.save_account_settings(settings).await;
        self.write_account(&[storage::AccountEdit::UniversalExpireTimer(seconds)]).await;
    }

    /// Desktop's `savePreferredReactions`: here, then the account record
    /// (`captureChange`), only that field changing.
    pub(super) async fn set_preferred_reactions(self: Rc<Self>, reactions: Vec<String>) {
        if !crate::preferred_reactions_syncable(&reactions) {
            return self.error("couldn't save your reactions", "too many, or too long");
        }
        let settings = crate::AccountSettings { preferred_reactions: Some(reactions.clone()), ..Default::default() };
        self.save_account_settings(settings).await;
        let Some(manager) = self.manager() else { return };
        match storage::write_account_reactions(&manager, &reactions).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't update your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// The default timer, if one is set.
    pub(super) async fn universal_expire_timer(&self) -> u32 {
        let value = self.db.setting(crate::SETTING_UNIVERSAL_EXPIRE_TIMER).await.ok().flatten();
        value.and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    /// Desktop's `maybeApplyUniversalTimer`: the first message we send in a
    /// 1:1 chat (Note to Self too, not the Signal chat) that has no timer
    /// and no message anyone wrote starts the default timer, as a timer
    /// change they see. (Desktop also shows a notice in such a chat
    /// beforehand; that isn't done here.)
    pub(super) async fn maybe_apply_universal_timer(&self, conversation: &ConversationId) {
        if conversation.as_direct().is_none() || conversation.is_signal() {
            return;
        }
        let seconds = self.universal_expire_timer().await;
        if seconds == 0 {
            return;
        }
        let Ok(Some(chat)) = self.db.conversation(conversation).await else { return };
        if chat.expire_timer != 0 || self.db.has_user_messages(conversation).await.unwrap_or(true) {
            return;
        }
        info!("applying the default timer to a new chat");
        self.set_direct_timer(conversation.clone(), seconds).await;
    }

    /// Desktop's "Who can see my number": the account record (everybody
    /// also makes us findable by number, as Desktop's
    /// `onWhoCanSeeMeChange` does), then our profile (the profile editor's
    /// write), which tells people whether they may see it.
    pub(super) async fn set_phone_number_sharing(self: Rc<Self>, everybody: bool) {
        let settings = crate::AccountSettings { phone_number_sharing: Some(everybody), ..Default::default() };
        self.save_account_settings(settings).await;
        let mut edits = vec![storage::AccountEdit::PhoneNumberSharing(everybody)];
        if everybody && !self.phone_number_discoverable().await {
            if !self.save_discoverable(true).await {
                return;
            }
            edits.push(storage::AccountEdit::UnlistedPhoneNumber(false));
            self.put_discoverability(true).await;
        }
        self.write_account(&edits).await;
        if let Err(error) = self.rewrite_profile().await {
            self.error("couldn't update your profile", format!("{error:#}"));
        }
    }

    /// Desktop's "Who can find me by number": the server
    /// (`setPhoneNumberDiscoverability`), then the account record. Nobody
    /// can't be chosen while everybody sees the number (the settings page
    /// doesn't offer it).
    pub(super) async fn set_phone_number_discoverable(self: Rc<Self>, discoverable: bool) {
        if !self.save_discoverable(discoverable).await {
            return;
        }
        self.put_discoverability(discoverable).await;
        self.write_account(&[storage::AccountEdit::UnlistedPhoneNumber(!discoverable)]).await;
    }

    /// Desktop's media auto-download choices: in effect at once, and saved.
    pub(super) async fn set_auto_download(&self, choices: crate::auto_download::AutoDownload) {
        crate::auto_download::set_current(choices);
        match serde_json::to_string(&choices) {
            Ok(json) => {
                if let Err(error) = self.db.set_setting(crate::auto_download::SETTING_AUTO_DOWNLOAD, &json).await {
                    self.error("failed to save media auto-download", error);
                }
            }
            Err(error) => warn!(%error, "failed to save media auto-download"),
        }
    }

    /// Desktop's "Import now" (`sendSyncRequests`): asks the phone for its
    /// contacts, configuration and blocked list.
    pub(super) fn request_syncs(&self) {
        use sync_message::request::Type;
        info!("asking the phone for its contacts");
        for kind in [Type::Contacts, Type::Configuration, Type::Blocked] {
            self.enqueue(Outbound::Sync {
                body: SyncMessage {
                    content: Some(sync_message::Content::Request(sync_message::Request { r#type: Some(kind as i32) })),
                    ..SyncMessage::with_padding(&mut rand::rng())
                },
            });
        }
    }

    /// The contact sync's photos (the phone's address book), saved as files
    /// named after their contents in `contact-avatars/`, for Desktop's "Use
    /// address book photos". Photos no one has any more are deleted.
    pub(super) async fn save_contact_avatars(&self) {
        let dir = self.config.data_dir.join(CONTACT_AVATARS_DIR);
        let Ok(contacts) = self.store.contacts().await else { return };
        let mut avatars = std::collections::BTreeMap::new();
        for contact in contacts.flatten() {
            let Some(avatar) = contact.avatar.as_ref().filter(|a| !a.reader.is_empty()) else { continue };
            let name = contact_avatar_name(&avatar.reader, &avatar.content_type);
            let path = dir.join(&name);
            if !path.exists() {
                let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &avatar.reader));
                if let Err(error) = written {
                    warn!(%error, "couldn't save a contact's photo");
                    continue;
                }
            }
            avatars.insert(Aci::from(contact.uuid).service_id_string(), path.to_string_lossy().into_owned());
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let kept: std::collections::HashSet<&str> = avatars.values().map(String::as_str).collect();
            for entry in entries.flatten() {
                if !kept.contains(entry.path().to_string_lossy().as_ref()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        match serde_json::to_string(&avatars) {
            Ok(json) => {
                if let Err(error) = self.db.set_setting(crate::SETTING_CONTACT_AVATARS, &json).await {
                    warn!(%error, "failed to save contacts' photos");
                }
            }
            Err(error) => warn!(%error, "failed to save contacts' photos"),
        }
        self.emit(Event::ConversationsChanged);
    }

    pub(super) async fn phone_number_discoverable(&self) -> bool {
        crate::setting_on(self.db.setting(crate::SETTING_PHONE_NUMBER_DISCOVERABLE).await.ok().flatten().as_deref())
    }

    /// Saves discoverability here; key transparency's self check waits a
    /// while, as Desktop's `onKnownIdentifierChange` does. `false` if it
    /// couldn't be saved.
    async fn save_discoverable(&self, discoverable: bool) -> bool {
        let value = if discoverable { "true" } else { "false" };
        if let Err(error) = self.db.set_setting(crate::SETTING_PHONE_NUMBER_DISCOVERABLE, value).await {
            self.error("failed to save who can find you", error);
            return false;
        }
        self.kt_identifier_changed(false).await;
        self.emit(Event::AccountSettingsChanged);
        true
    }

    async fn put_discoverability(&self, discoverable: bool) {
        let Some(manager) = self.manager() else { return };
        let result = async {
            let response = storage::push_service(&manager)?
                .request(
                    reqwest::Method::PUT,
                    Endpoint::service("/v2/accounts/phone_number_discoverability"),
                    HttpAuthOverride::NoOverride,
                )?
                .json(&serde_json::json!({ "discoverableByPhoneNumber": discoverable }))
                .send()
                .await?;
            response.error_for_status()?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            self.error("couldn't change who can find you by number", format!("{error:#}"));
        }
    }
}

/// Where the contact sync's photos are kept.
pub(crate) const CONTACT_AVATARS_DIR: &str = "contact-avatars";

/// A contact photo's file name: a hash of its bytes, so a changed photo is
/// a new file.
fn contact_avatar_name(bytes: &[u8], content_type: &str) -> String {
    use sha2::Digest as _;
    let ext = match content_type {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "jpg",
    };
    format!("{}.{ext}", hex::encode(&sha2::Sha256::digest(bytes)[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_photos_are_named_after_their_contents() {
        let a = contact_avatar_name(b"photo one", "image/jpeg");
        assert!(a.ends_with(".jpg") && a.len() == 32 + 4, "{a}");
        assert_eq!(a, contact_avatar_name(b"photo one", "image/jpeg"));
        assert_ne!(a, contact_avatar_name(b"photo two", "image/jpeg"));
        assert!(contact_avatar_name(b"x", "image/png").ends_with(".png"));
    }
}
