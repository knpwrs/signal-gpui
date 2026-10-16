//! Key transparency, run when Desktop runs it
//! (`ts/services/keyTransparency.preload.ts`): a contact's check when
//! "Verify automatically" is pressed in their safety number screen, our own
//! account's check every week (and before a contact's, until it has run
//! once), and resets when our identifiers change. The checks themselves
//! are in `crate::key_transparency`.

use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, Shared};
use libsignal_net_chat::api::keytrans::{AccountDataField, CheckMode};
use presage::libsignal_service::protocol::{E164, Username};

use super::*;
use crate::key_transparency::{self as kt, ChatApi, Failure, Health, Websocket};

/// Our self check while it runs, shared by everyone waiting on it (Desktop's
/// `TaskDeduplicator`).
pub(super) type SelfCheck = Shared<LocalBoxFuture<'static, Result<(), Failure>>>;

/// How long the self check waits for a Storage Service read after an
/// intermittent failure (Desktop's `STORAGE_SERVICE_TIMEOUT`).
const STORAGE_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

fn parse_e164(phone: &str) -> Option<E164> {
    phone.parse().ok()
}

impl Actor {
    async fn kt_chat(&self) -> Result<ChatApi<Websocket>, Failure> {
        let manager = self.manager().ok_or_else(|| Failure::Other("not linked".into()))?;
        let ws = manager.unauthenticated_websocket().await.map_err(|e| Failure::Network(e.to_string()))?;
        Ok(ChatApi(Websocket::new(ws)))
    }

    /// Whether the safety number screen offers automatic key verification
    /// for them: on, and (Desktop's `isKeyTransparencyAvailable` in
    /// `SmartSafetyNumberViewer`) we know their phone number.
    pub(super) async fn auto_verify(&self, conversation: &ConversationId) -> crate::AutoVerify {
        if !kt::is_available(&self.db).await {
            return crate::AutoVerify::Off;
        }
        let phone = self.db.conversation(conversation).await.ok().flatten().and_then(|c| c.phone);
        if phone.as_deref().and_then(parse_e164).is_some() {
            crate::AutoVerify::Available
        } else {
            crate::AutoVerify::Unavailable
        }
    }

    /// "Verify automatically": checks them, then that our own account
    /// checks out. Answered with `Event::KeyTransparency`; a check already
    /// running for another chat is stopped (Desktop stops it when the
    /// screen closes).
    pub(super) fn check_key_transparency(self: &Rc<Self>, conversation: ConversationId) {
        let this = self.clone();
        let task = tokio::task::spawn_local(async move {
            let result = this.kt_contact_check(&conversation).await;
            if let Err(failure) = &result {
                warn!(%failure, "key transparency check failed");
            }
            this.emit(Event::KeyTransparency { conversation, verified: result.is_ok() });
        });
        if let Some(old) = self.kt_contact.borrow_mut().replace(task) {
            old.abort();
        }
    }

    /// The safety number screen closed: its check stops, as Desktop's
    /// abort signal stops it. (Our data from a step that finished stays.)
    pub(super) fn cancel_key_transparency(&self) {
        if let Some(task) = self.kt_contact.borrow_mut().take() {
            task.abort();
        }
    }

    /// Desktop's `KeyTransparency.check`.
    async fn kt_contact_check(self: &Rc<Self>, conversation: &ConversationId) -> Result<(), Failure> {
        if !kt::is_available(&self.db).await {
            return Err(Failure::Other("automatic key verification is off".into()));
        }
        let (_, _, aci, identity) = self.identities(conversation).await.map_err(|e| Failure::Other(format!("{e:#}")))?;
        // Their number counts only with their access key, as Desktop's
        // `e164Info`.
        let phone = self.db.conversation(conversation).await.ok().flatten().and_then(|c| c.phone);
        let access_key = self.profile_key_of(&ServiceId::from(aci)).await.map(|key| key.derive_access_key());
        let e164 = phone.as_deref().and_then(parse_e164).zip(access_key);
        let request =
            kt::Request { aci, identity_key: *identity.public_key(), e164, username_hash: None, mode: CheckMode::ContactCheck };
        let chat = self.kt_chat().await?;
        kt::check(&self.db, &chat, &request).await?;
        match kt::self_health(&self.db).await {
            None => self.kt_self_check().await,
            Some(Health::Ok) => Ok(()),
            Some(_) => Err(Failure::Other("our own account's check failed".into())),
        }
    }

    /// Desktop's `selfCheck`: one at a time, shared by whoever asks while
    /// it runs.
    pub(super) async fn kt_self_check(self: &Rc<Self>) -> Result<(), Failure> {
        let shared = {
            let mut running = self.kt_self.borrow_mut();
            match running.as_ref() {
                Some(check) => check.clone(),
                None => {
                    let this = self.clone();
                    let check = async move {
                        let result = this.run_self_check().await;
                        this.kt_self.borrow_mut().take();
                        result
                    }
                    .boxed_local()
                    .shared();
                    *running = Some(check.clone());
                    check
                }
            }
        };
        shared.await
    }

    /// Desktop's `#selfCheck`: checks our own account and records how it
    /// went (`keyTransparencySelfHealth`). A check that now fails (and
    /// didn't before) brings up Desktop's error dialog.
    async fn run_self_check(self: &Rc<Self>) -> Result<(), Failure> {
        if !kt::is_available(&self.db).await {
            info!("key transparency self check: off");
            return Ok(());
        }
        let Some(manager) = self.manager() else { return Ok(()) };
        let data = manager.registration_data();
        let aci = data.service_ids.aci();
        let store = presage::store::Store::aci_protocol_store(&self.store);
        let identity = match store.get_identity_key_pair().await {
            Ok(pair) => *pair.identity_key(),
            Err(error) => {
                warn!(%error, "key transparency self check: no identity key pair");
                return Ok(());
            }
        };
        info!("key transparency: running self check");
        let discoverable = self.db.setting(kt::SETTING_E164_DISCOVERABLE).await.ok().flatten().as_deref() == Some("true");
        let access_key = data.profile_key().derive_access_key();
        let e164 = parse_e164(&data.phone_number.to_string()).map(|e164| (e164, access_key));
        let username = self.db.setting(kt::SETTING_USERNAME).await.ok().flatten().filter(|u| !u.is_empty());
        let username_hash = username.and_then(|u| Username::new(&u).ok()).map(|u| u.hash());

        let old = kt::self_health(&self.db).await;
        if old == Some(Health::Intermittent) {
            // Be sure what we ask about is current (Desktop runs a Storage
            // Service sync and waits for it).
            let _ = tokio::time::timeout(STORAGE_SYNC_TIMEOUT, self.sync_storage(&manager)).await;
        }
        let request = kt::Request {
            aci,
            identity_key: *identity.public_key(),
            e164,
            username_hash,
            mode: CheckMode::SelfCheck { is_e164_discoverable: discoverable },
        };
        let result = match self.kt_chat().await {
            Ok(chat) => kt::check(&self.db, &chat, &request).await,
            Err(failure) => Err(failure),
        };
        let failure = match result {
            Ok(()) => {
                if let Err(error) = kt::set_self_health(&self.db, Some(Health::Ok)).await {
                    warn!(%error, "failed to save the key transparency self check");
                }
                info!("key transparency self check: ok");
                return Ok(());
            }
            Err(failure) => failure,
        };
        let new = kt::next_health(old, &failure);
        warn!(%failure, ?new, "key transparency: our own account's check failed");
        if let Err(error) = kt::set_self_health(&self.db, new).await {
            warn!(%error, "failed to save the key transparency self check");
        }
        if old != Some(Health::Fail) && new == Some(Health::Fail) {
            self.emit(Event::KeyTransparencyFailed);
        }
        if new == Some(Health::Intermittent) {
            self.kt_run_at(now_millis() + kt::INTERMITTENT_ERROR_RETRY_DELAY.as_millis() as i64).await;
        }
        Err(failure)
    }

    async fn kt_last_self_check(&self) -> Option<i64> {
        self.db.setting(kt::SETTING_LAST_SELF_CHECK).await.ok().flatten().and_then(|v| v.parse().ok())
    }

    async fn kt_set_last_self_check(&self, at: i64) {
        if let Err(error) = self.db.set_setting(kt::SETTING_LAST_SELF_CHECK, &at.to_string()).await {
            warn!(%error, "failed to save when the key transparency self check runs");
        }
        self.kt_schedule.notify_one();
    }

    /// `CheckScheduler.runAt`.
    async fn kt_run_at(&self, at: i64) {
        self.kt_set_last_self_check(kt::last_for_run_at(at)).await;
    }

    /// `CheckScheduler.delayBy`.
    async fn kt_delay_by(&self, delay: Duration) {
        let last = self.kt_last_self_check().await;
        self.kt_set_last_self_check(kt::last_for_delay(last, now_millis(), delay)).await;
    }

    /// After linking, the first self check comes in five minutes (Desktop's
    /// `onRegistrationDone`).
    pub(super) async fn kt_linked(&self) {
        self.kt_run_at(now_millis() + kt::KNOWN_IDENTIFIER_CHANGE_DELAY.as_millis() as i64).await;
    }

    /// Desktop's `onKnownIdentifierChange`: our username changed (its field
    /// in our stored data is forgotten) or our access key or number
    /// discoverability did; either way the self check waits a while.
    pub(super) async fn kt_identifier_changed(&self, username: bool) {
        if username
            && let Some(aci) = Aci::parse_from_service_id_string(&self.self_aci())
            && let Err(failure) = kt::reset_field(&self.db, &aci, AccountDataField::UsernameHash).await
        {
            warn!(%failure, "key transparency: couldn't forget our old username");
        }
        self.kt_delay_by(kt::KNOWN_IDENTIFIER_CHANGE_DELAY).await;
    }

    /// Desktop's `CheckScheduler` for the self check: weekly, at the time
    /// kept in settings (which `kt_run_at` and `kt_delay_by` move).
    pub(super) async fn key_transparency_loop(self: Rc<Self>) {
        loop {
            let last = self.kt_last_self_check().await;
            let due = kt::next_self_check(last, now_millis(), rand::random(), rand::random());
            let delay = Duration::from_millis(due.saturating_sub(now_millis()).max(0) as u64);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                // Rescheduled: work it out again.
                _ = self.kt_schedule.notified() => continue,
            }
            let before = self.db.setting(kt::SETTING_LAST_SELF_CHECK).await.ok().flatten();
            // Desktop's scheduler ignores the check's errors.
            let _ = self.kt_self_check().await;
            // Unless the check moved the next run itself, it ran now.
            if self.db.setting(kt::SETTING_LAST_SELF_CHECK).await.ok().flatten() == before {
                self.kt_set_last_self_check(now_millis()).await;
            }
        }
    }

    /// Automatic key verification on or off, here and in the account record
    /// (`automaticKeyVerificationDisabled`); off forgets what was verified.
    pub(super) async fn set_key_transparency_disabled(self: Rc<Self>, disabled: bool) {
        self.apply_key_transparency_disabled(disabled).await;
        self.emit(Event::StorySettingsChanged);
        let Some(manager) = self.manager() else { return };
        match storage::write_account(&manager, &[storage::AccountEdit::AutomaticKeyVerificationDisabled(disabled)]).await {
            Ok(()) => self.storage_written(),
            Err(error) => {
                self.error("couldn't update your other devices", format!("{error:#}"));
                self.sync_storage(&manager).await;
            }
        }
    }

    /// Saves the setting; turning it off forgets everything verified
    /// (Desktop's `disable`).
    pub(super) async fn apply_key_transparency_disabled(&self, disabled: bool) {
        if let Err(error) = self.db.set_setting(kt::SETTING_DISABLED, if disabled { "true" } else { "false" }).await {
            return self.error("failed to save automatic key verification", error);
        }
        if disabled {
            self.cancel_key_transparency();
            if let Err(error) = kt::forget_all(&self.db).await {
                warn!(%error, "failed to forget key transparency data");
            }
        }
    }

    /// The account record's fields key transparency depends on (Desktop's
    /// `mergeAccountRecord`): our username, whether our number is
    /// discoverable, and whether automatic key verification is off.
    pub(super) async fn apply_account_identifiers(&self, account: &storage::AccountIdentifiers) {
        let disabled = kt::is_disabled(&self.db).await;
        if account.key_transparency_disabled != disabled {
            self.apply_key_transparency_disabled(account.key_transparency_disabled).await;
        }
        let username = account.username.clone().unwrap_or_default();
        let old_username = self.db.setting(kt::SETTING_USERNAME).await.ok().flatten().unwrap_or_default();
        let username_changed = username != old_username;
        if username_changed && let Err(error) = self.db.set_setting(kt::SETTING_USERNAME, &username).await {
            warn!(%error, "failed to save our username");
        }
        let discoverable = if account.unlisted_phone_number { "false" } else { "true" };
        let old_discoverable = self.db.setting(kt::SETTING_E164_DISCOVERABLE).await.ok().flatten();
        if old_discoverable.as_deref() != Some(discoverable)
            && let Err(error) = self.db.set_setting(kt::SETTING_E164_DISCOVERABLE, discoverable).await
        {
            warn!(%error, "failed to save our number's discoverability");
        }
        if username_changed {
            self.kt_identifier_changed(true).await;
        }
        // Desktop puts the check off whenever the record carries our profile
        // key (the access key comes from it) and when discoverability
        // changes; `delayBy` only moves a check due in the next 5 minutes.
        if account.has_profile_key || old_discoverable.as_deref() != Some(discoverable) {
            self.kt_delay_by(kt::KNOWN_IDENTIFIER_CHANGE_DELAY).await;
        }
    }
}
