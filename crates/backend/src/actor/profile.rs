//! Profiles, usernames and badges, as Signal Desktop handles them: others'
//! profiles fetched when shown (`throttledGetProfiles`) and in the daily
//! photo check; ours written from the profile editor
//! (`ts/services/writeProfile.preload.ts`) and read from the account
//! record (`mergeAccountRecord`); our username and its link
//! (`ts/services/username.preload.ts`); badge images downloaded as
//! `ts/badges/badgeImageFileDownloader.preload.ts` does.

use super::*;
use crate::profiles::{self, AvatarChange, ProfileEdit};
use crate::storage::{AccountFieldEdit, OwnProfileRecord};
use crate::usernames::{self, Reservation, ReserveRequest, SETTING_USERNAME_LINK, UsernameLink, UsernameProblem};

/// Settings key: our profile key from the account record (hex). The
/// primary makes a new one now and then (when blocking someone, say); a
/// linked device follows the account record's, as Desktop's
/// `mergeAccountRecord` does.
const SETTING_PROFILE_KEY: &str = "account.profile_key";
/// Settings key: our photo's CDN path (Desktop's `avatarUrl` item).
const SETTING_AVATAR_URL: &str = "account.avatar_url";

/// Someone's profile is fetched again for being shown at most this often
/// (Desktop's `throttledGetProfiles`).
const PROFILE_SHOWN_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Badge images downloaded per run; the rest wait for the next.
const BADGE_DOWNLOADS_PER_RUN: usize = 30;

impl Actor {
    /// Our profile key: the account record's when Storage Service gave
    /// one, else the one from linking.
    pub(super) async fn our_profile_key(&self, manager: &SignalManager) -> ProfileKey {
        let saved = self
            .db
            .setting(SETTING_PROFILE_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|hex| hex::decode(hex).ok())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        saved.map(ProfileKey::create).unwrap_or_else(|| manager.registration_data().profile_key())
    }

    /// Saves what a profile fetch said: name, about, badges and the badges'
    /// descriptions.
    pub(super) async fn save_remote_profile(&self, sid: &str, remote: &profiles::RemoteProfile) {
        if let Err(error) = self.db.save_badges(&remote.badges).await {
            warn!(%error, "failed to save badges");
        }
        let saved = self.db.save_fetched_profile_noting_name(sid, &remote.profile, now_millis()).await;
        // Desktop's `setEncryptedProfileName`: a notice when the name
        // changed from one they had, never for ours (`addProfileChange`).
        if let Ok((_, Some((old_name, new_name)))) = &saved
            && sid != self.self_aci()
        {
            let notice = signal_store::Notice::ProfileNameChanged {
                who: sid.to_owned(),
                old_name: old_name.clone(),
                new_name: new_name.clone(),
            };
            self.notice_about(sid, notice).await;
        }
        match saved.map(|(changed, _)| changed) {
            Ok(true) => {
                self.emit(Event::ProfileChanged(sid.to_owned()));
                if sid == self.self_aci() {
                    self.emit(Event::OwnProfileChanged);
                }
            }
            Ok(false) => {}
            Err(error) => warn!(%error, "failed to save a profile"),
        }
    }

    /// `Command::LoadProfile`: their profile and photo, fetched again unless
    /// that was done in the last five minutes.
    pub(super) async fn load_profile(self: Rc<Self>, sid: String) {
        let Some(aci) = Aci::parse_from_service_id_string(&sid) else { return };
        if sid == signal_store::SIGNAL_ACI {
            return;
        }
        {
            let mut shown = self.profile_shown.borrow_mut();
            if shown.get(&sid).is_some_and(|at| at.elapsed() < PROFILE_SHOWN_INTERVAL) {
                return;
            }
            shown.insert(sid.clone(), Instant::now());
        }
        self.refresh_profile(aci).await;
    }

    /// Fetches their profile now and brings their name, about, badges and
    /// photo up to date.
    async fn refresh_profile(self: &Rc<Self>, aci: Aci) {
        let Some(manager) = self.manager() else { return };
        let sid = aci.service_id_string();
        let ours = sid == self.self_aci();
        let key = if ours { Some(self.our_profile_key(&manager).await) } else { self.profile_key_of(&ServiceId::from(aci)).await };
        let Some(key) = key else { return };
        let remote = match profiles::fetch(&manager, aci, key).await {
            Ok(remote) => remote,
            Err(error) => return debug!(error = %format!("{error:#}"), "profile fetch failed"),
        };
        self.save_remote_profile(&sid, &remote).await;
        let dir = self.config.data_dir.join("avatars");
        if std::fs::create_dir_all(&dir).is_ok() {
            let saved = self.db.person_avatars().await.unwrap_or_default().into_iter().find(|(s, _)| *s == sid).map(|(_, p)| p);
            let outcome = self.apply_person_photo(&manager, &sid, remote.avatar.clone(), key, saved.as_deref(), &dir).await;
            if outcome == PhotoOutcome::Changed {
                self.emit(Event::ConversationsChanged);
                self.emit(Event::ProfileChanged(sid.clone()));
                if ours {
                    self.emit(Event::OwnProfileChanged);
                }
            }
        }
        self.download_badges();
    }

    /// Another of our devices changed our profile: fetch it again.
    pub(super) fn local_profile_changed(self: &Rc<Self>, _manager: &SignalManager) {
        let Some(aci) = Aci::parse_from_service_id_string(&self.self_aci()) else { return };
        let this = self.clone();
        tokio::task::spawn_local(async move { this.refresh_profile(aci).await });
    }

    /// What the account record says about us (Desktop's
    /// `mergeAccountRecord`): our profile key, name, photo, number sharing
    /// and username link.
    pub(super) async fn apply_own_profile_record(self: &Rc<Self>, manager: &SignalManager, record: OwnProfileRecord) {
        let me = self.self_aci();
        let mut changed = false;
        if let Some(key) = record.profile_key {
            let hex = hex::encode(key);
            if self.db.setting(SETTING_PROFILE_KEY).await.ok().flatten().as_deref() != Some(hex.as_str()) {
                if key != manager.registration_data().profile_key().get_bytes() {
                    info!("the account record has a profile key other than the one from linking");
                }
                let _ = self.db.set_setting(SETTING_PROFILE_KEY, &hex).await;
            }
        }
        // (Whether our number is shared is saved with the account's other
        // settings, `crate::SETTING_PHONE_NUMBER_SHARING`.)
        if !record.given_name.is_empty() {
            changed |= self.db.set_profile_name(&me, &record.given_name, &record.family_name).await.unwrap_or(false);
        }
        let link = record.username_link.as_ref().map(serde_json::to_string).transpose().ok().flatten();
        let old_link = self.db.setting(SETTING_USERNAME_LINK).await.ok().flatten();
        if link != old_link {
            changed = true;
            let _ = match &link {
                Some(link) => self.db.set_setting(SETTING_USERNAME_LINK, link).await,
                None => self.db.remove_setting(SETTING_USERNAME_LINK).await,
            };
        }
        // Desktop's `setAndMaybeFetchProfileAvatar`: a new path, a new photo.
        let old_path = self.db.setting(SETTING_AVATAR_URL).await.ok().flatten();
        if record.avatar_url_path != old_path {
            let dir = self.config.data_dir.join("avatars");
            if std::fs::create_dir_all(&dir).is_ok() {
                let key = self.our_profile_key(manager).await;
                let saved = self.db.person_avatars().await.unwrap_or_default().into_iter().find(|(s, _)| *s == me).map(|(_, p)| p);
                let outcome = self.apply_person_photo(manager, &me, record.avatar_url_path.clone(), key, saved.as_deref(), &dir).await;
                changed |= outcome == PhotoOutcome::Changed;
                // Only once it's here, so a failed download is tried again
                // at the next sync.
                if outcome != PhotoOutcome::DownloadFailed {
                    let _ = match &record.avatar_url_path {
                        Some(path) => self.db.set_setting(SETTING_AVATAR_URL, path).await,
                        None => self.db.remove_setting(SETTING_AVATAR_URL).await,
                    };
                }
            }
        }
        if changed {
            self.emit(Event::OwnProfileChanged);
        }
    }

    /// Downloads badge images not downloaded yet, a bounded number per run
    /// (Desktop's `checkForFilesToDownload`, without its queue).
    pub(super) fn download_badges(self: &Rc<Self>) {
        if self.badge_downloads.replace(true) {
            return;
        }
        let this = self.clone();
        tokio::task::spawn_local(async move {
            let got = this.download_some_badges().await;
            this.badge_downloads.set(false);
            if got > 0 {
                this.emit(Event::BadgesChanged);
            }
        });
    }

    async fn download_some_badges(&self) -> usize {
        let badges = self.db.badges().await.unwrap_or_default();
        let files = self.db.badge_image_files().await.unwrap_or_default();
        let mut missing: Vec<String> = badges
            .iter()
            .flat_map(|b| b.urls())
            .filter(|url| files.get(*url).is_none_or(|path| !std::path::Path::new(path).exists()))
            .map(str::to_owned)
            .collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            return 0;
        }
        let dir = self.config.data_dir.join("badges");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            warn!(%error, "failed to create the badges directory");
            return 0;
        }
        let client = match crate::release_notes::client() {
            Ok(client) => client,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "no client for badge images");
                return 0;
            }
        };
        let mut got = 0;
        for url in missing.iter().take(BADGE_DOWNLOADS_PER_RUN) {
            let bytes = match profiles::download_badge_image(&client, url).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    debug!(error = %format!("{error:#}"), "badge image download failed");
                    continue;
                }
            };
            use sha2::Digest as _;
            let ext = if url.ends_with(".svg") { "svg" } else { "png" };
            let path = dir.join(format!("{}.{ext}", &hex::encode(sha2::Sha256::digest(url.as_bytes()))[..32]));
            if std::fs::write(&path, &bytes).is_err() {
                continue;
            }
            if let Some(path) = path.to_str()
                && self.db.set_badge_image_file(url, path).await.is_ok()
            {
                got += 1;
            }
        }
        if missing.len() > BADGE_DOWNLOADS_PER_RUN {
            info!(left = missing.len() - BADGE_DOWNLOADS_PER_RUN, "more badge images to download later");
        }
        got
    }

    /// Tells our other devices our profile changed (Desktop's
    /// `getFetchLocalProfileSyncMessage`).
    fn local_profile_written(&self) {
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::FetchLatest(sync_message::FetchLatest {
                    r#type: Some(sync_message::fetch_latest::Type::LocalProfile as i32),
                })),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Writes our profile's fields to the account record; a failure is
    /// only logged, as Desktop's storage upload is a job of its own.
    async fn write_own_record(&self, manager: &SignalManager, edits: &[AccountFieldEdit]) {
        match storage::write_account_fields(manager, edits).await {
            Ok(()) => self.storage_written(),
            Err(error) => warn!(error = %format!("{error:#}"), "couldn't write our profile to the account record"),
        }
    }

    /// `Command::SetProfile` (Desktop's `writeProfile`).
    pub(super) async fn set_profile(self: Rc<Self>, edit: ProfileEdit) {
        let _edit = self.profile_edit.lock().await;
        let result = self.write_profile(&edit).await;
        if let Err(error) = &result {
            warn!(error = %format!("{error:#}"), "couldn't write our profile");
        }
        self.emit(Event::ProfileSaved(result.map_err(|_| "Your profile could not be updated. Please try again.".to_owned())));
    }

    /// Desktop's `writeProfile` after "Who can see my number" changes
    /// (`onWhoCanSeeMeChange`): our profile as it is, its
    /// `phoneNumberSharing` from the setting just saved.
    pub(super) async fn rewrite_profile(self: &Rc<Self>) -> anyhow::Result<()> {
        let _edit = self.profile_edit.lock().await;
        let manager = self.manager().context("not linked")?;
        let aci = Aci::parse_from_service_id_string(&self.self_aci()).context("no ACI")?;
        let key = self.our_profile_key(&manager).await;
        let current = profiles::fetch(&manager, aci, key).await.context("reading our profile first")?;
        let edit = ProfileEdit {
            given_name: current.profile.given_name.clone(),
            family_name: current.profile.family_name.clone(),
            about: current.profile.about.clone(),
            about_emoji: current.profile.about_emoji.clone(),
            avatar: AvatarChange::Keep,
        };
        self.write_profile(&edit).await
    }

    async fn write_profile(self: &Rc<Self>, edit: &ProfileEdit) -> anyhow::Result<()> {
        let manager = self.manager().context("not linked")?;
        let me = self.self_aci();
        let aci = Aci::parse_from_service_id_string(&me).context("no ACI")?;
        let key = self.our_profile_key(&manager).await;
        // Desktop fetches our profile first, so the write keeps our badges'
        // visibility and our payment address.
        let current = profiles::fetch(&manager, aci, key).await.context("reading our profile first")?;
        let badge_ids: Vec<String> =
            current.profile.badges.iter().filter(|b| b.is_visible()).map(|b| b.id.clone()).collect();
        let sharing = crate::setting_on(self.db.setting(crate::SETTING_PHONE_NUMBER_SHARING).await.ok().flatten().as_deref());
        let kept = profiles::Kept {
            has_avatar: current.avatar.is_some(),
            badge_ids: &badge_ids,
            payment_address: current.payment_address.as_deref(),
            sharing_number: sharing,
        };
        let request = profiles::profile_request(edit, aci, key, &kept, &mut rand::rng())?;
        let uploaded = profiles::write(&manager, request).await?;
        let avatar_path = match &edit.avatar {
            AvatarChange::Keep => current.avatar.clone(),
            AvatarChange::Remove => None,
            AvatarChange::New(_) => uploaded,
        };

        // Ours here, as written.
        let saved = signal_store::FetchedProfile {
            given_name: edit.given_name.trim().to_owned(),
            family_name: edit.family_name.trim().to_owned(),
            about: edit.about.trim().to_owned(),
            about_emoji: if edit.about.trim().is_empty() { String::new() } else { edit.about_emoji.clone() },
            badges: current.profile.badges.clone(),
        };
        if let Err(error) = self.db.save_fetched_profile(&me, &saved, now_millis()).await {
            warn!(%error, "failed to save our profile");
        }
        self.save_own_photo(&me, &edit.avatar, avatar_path.as_deref()).await;
        let _ = match &avatar_path {
            Some(path) => self.db.set_setting(SETTING_AVATAR_URL, path).await,
            None => self.db.remove_setting(SETTING_AVATAR_URL).await,
        };
        self.emit(Event::OwnProfileChanged);

        self.write_own_record(
            &manager,
            &[
                AccountFieldEdit::Name { given: saved.given_name.clone(), family: saved.family_name.clone() },
                AccountFieldEdit::AvatarPath(avatar_path),
            ],
        )
        .await;
        self.local_profile_written();
        Ok(())
    }

    /// Keeps the photo we just wrote (Desktop's `writeNewAttachmentData`),
    /// named after its CDN path like downloaded ones.
    async fn save_own_photo(&self, me: &str, change: &AvatarChange, path: Option<&str>) {
        let old = self.db.person_avatars().await.unwrap_or_default().into_iter().find(|(s, _)| s == me).map(|(_, p)| p);
        match (change, path) {
            (AvatarChange::Keep, _) => {}
            (AvatarChange::New(bytes), Some(path)) => {
                let dir = self.config.data_dir.join("avatars");
                let ext = if bytes.starts_with(b"\x89PNG") { "png" } else { "jpg" };
                let file = dir.join(format!("{}.{ext}", profiles::file_stem(me, path)));
                if std::fs::create_dir_all(&dir).is_err() || std::fs::write(&file, bytes.as_slice()).is_err() {
                    return warn!("failed to save our new photo");
                }
                let Some(file) = file.to_str() else { return };
                if self.db.set_person_avatar(me, file).await.is_ok()
                    && let Some(old) = old.filter(|old| old != file)
                {
                    let _ = std::fs::remove_file(old);
                }
            }
            (AvatarChange::New(_), None) | (AvatarChange::Remove, _) => {
                if let Some(old) = old {
                    let _ = std::fs::remove_file(old);
                }
                let _ = self.db.remove_person_avatar(me).await;
            }
        }
    }

    async fn our_username(&self) -> Option<String> {
        self.db.setting(crate::key_transparency::SETTING_USERNAME).await.ok().flatten().filter(|u| !u.is_empty())
    }

    async fn our_username_link(&self) -> Option<UsernameLink> {
        self.db.setting(SETTING_USERNAME_LINK).await.ok().flatten().and_then(|json| serde_json::from_str(&json).ok())
    }

    /// Saves our username (or none) and its link here; key transparency
    /// starts over for the username (Desktop's `onKnownIdentifierChange`).
    async fn save_username(&self, username: Option<&str>, link: Option<&UsernameLink>) {
        let old = self.our_username().await;
        let _ = self.db.set_setting(crate::key_transparency::SETTING_USERNAME, username.unwrap_or_default()).await;
        let _ = match link.map(serde_json::to_string).transpose().ok().flatten() {
            Some(json) => self.db.set_setting(SETTING_USERNAME_LINK, &json).await,
            None => self.db.remove_setting(SETTING_USERNAME_LINK).await,
        };
        if old.as_deref() != username {
            self.kt_identifier_changed(true).await;
        }
        self.emit(Event::OwnProfileChanged);
    }

    /// `Command::ReserveUsername`: a newer one aborts an older one still
    /// waiting (Desktop's abort controller).
    pub(super) fn reserve_username(self: &Rc<Self>, nickname: String, discriminator: Option<String>) {
        let this = self.clone();
        let task = tokio::task::spawn_local(async move {
            let result = this.reserve(&nickname, discriminator.as_deref()).await;
            this.emit(Event::UsernameReserved { nickname, discriminator, result });
        });
        if let Some(old) = self.username_reservation.borrow_mut().replace(task) {
            old.abort();
        }
    }

    async fn reserve(&self, nickname: &str, discriminator: Option<&str>) -> Result<Reservation, UsernameProblem> {
        let manager = self.manager().ok_or(UsernameProblem::General)?;
        let previous = self.our_username().await;
        let request =
            usernames::reserve_request(nickname, discriminator, previous.as_deref(), usernames::limits(), &mut rand::rng())?;
        match request {
            ReserveRequest::CaseChange(reservation) => Ok(reservation),
            ReserveRequest::Candidates(candidates) => usernames::reserve(&manager, candidates, previous).await,
        }
    }

    /// `Command::ConfirmUsername` (Desktop's `confirmUsername`).
    pub(super) async fn confirm_username(self: Rc<Self>, reservation: Reservation) {
        let _edit = self.profile_edit.lock().await;
        let result = async {
            let manager = self.manager().ok_or(UsernameProblem::General)?;
            if self.our_username().await != reservation.previous {
                warn!("our username changed on another device while choosing one");
                return Err(UsernameProblem::General);
            }
            let previous_link = self.our_username_link().await;
            let link = usernames::confirm(&manager, &reservation, previous_link.as_ref()).await?;
            self.save_username(Some(&reservation.username), Some(&link)).await;
            self.write_own_record(
                &manager,
                &[AccountFieldEdit::Username(Some(reservation.username.clone())), AccountFieldEdit::UsernameLink(Some(link))],
            )
            .await;
            self.local_profile_written();
            Ok(Some(reservation.username.clone()))
        }
        .await;
        self.emit(Event::UsernameSaved(result));
    }

    /// `Command::DeleteUsername` (Desktop's `deleteUsername`).
    pub(super) async fn delete_username(self: Rc<Self>) {
        let _edit = self.profile_edit.lock().await;
        let result = async {
            let manager = self.manager().ok_or(UsernameProblem::General)?;
            usernames::delete(&manager).await.map_err(|error| {
                warn!(error = %format!("{error:#}"), "couldn't delete our username");
                UsernameProblem::General
            })?;
            self.save_username(None, None).await;
            self.write_own_record(&manager, &[AccountFieldEdit::Username(None), AccountFieldEdit::UsernameLink(None)]).await;
            self.local_profile_written();
            Ok(None)
        }
        .await;
        self.emit(Event::UsernameSaved(result));
    }

    /// `Command::ResetUsernameLink` (Desktop's `resetLink`): a new handle
    /// and entropy, the color kept.
    pub(super) async fn reset_username_link(self: Rc<Self>) {
        let _edit = self.profile_edit.lock().await;
        let result = async {
            let manager = self.manager().context("not linked")?;
            let username = self.our_username().await.context("no username")?;
            let color = self.our_username_link().await.map_or(0, |l| l.color);
            let (entropy, encrypted) = usernames::encrypt_for_link(&username, None, &mut rand::rng())?;
            let server_id = usernames::put_link(&manager, &encrypted, false).await.map_err(|e| match e {
                usernames::LinkError::NoUsername => anyhow::anyhow!("the server has no username for us"),
                usernames::LinkError::Other(error) => error,
            })?;
            let link = UsernameLink { entropy: entropy.to_vec(), server_id, color };
            self.save_username(Some(&username), Some(&link)).await;
            self.write_own_record(&manager, &[AccountFieldEdit::UsernameLink(Some(link))]).await;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = &result {
            warn!(error = %format!("{error:#}"), "couldn't reset our username link");
        }
        self.emit(Event::UsernameLinkReset(result.map_err(|_| "Your QR code and link couldn't be reset. Check your connection and try again.".to_owned())));
    }

    /// `Command::SetUsernameLinkColor` (Desktop's `setUsernameLinkColor`):
    /// here at once, then in the account record.
    pub(super) async fn set_username_link_color(self: Rc<Self>, color: i32) {
        let _edit = self.profile_edit.lock().await;
        let Some(mut link) = self.our_username_link().await else { return };
        if link.color == color || !(0..=8).contains(&color) {
            return;
        }
        link.color = color;
        if let Ok(json) = serde_json::to_string(&link) {
            let _ = self.db.set_setting(SETTING_USERNAME_LINK, &json).await;
        }
        self.emit(Event::OwnProfileChanged);
        if let Some(manager) = self.manager() {
            self.write_own_record(&manager, &[AccountFieldEdit::UsernameLink(Some(link))]).await;
        }
    }
}
