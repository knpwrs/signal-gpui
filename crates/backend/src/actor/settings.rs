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
