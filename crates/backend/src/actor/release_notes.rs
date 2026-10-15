//! Fetching release notes into the Signal chat on Desktop's schedule
//! (`ReleaseNoteAndMegaphoneFetcher`): once a day, and again three hours
//! after a failure. See `crate::release_notes` for what's fetched.

use signal_store::{CallToAction, ConversationKind, ConversationUpdate, SIGNAL_ACI};

use super::*;
use crate::release_notes;

/// Desktop's `FETCH_INTERVAL` and `ERROR_RETRY_DELAY`.
const FETCH_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
const ERROR_RETRY: Duration = Duration::from_secs(3 * 60 * 60);
/// Desktop's storage keys, as settings.
const SETTING_NEXT_FETCH: &str = "release_notes.next_fetch";
const SETTING_MANIFEST_HASH: &str = "release_notes.manifest_hash";
const SETTING_WATERMARK: &str = "release_notes.watermark";
/// The Desktop version the manifest was last read for: a new one reads it
/// again, as a new Desktop version does there.
const SETTING_DESKTOP_VERSION: &str = "release_notes.desktop_version";

impl Actor {
    /// Runs for the life of the account: fetches when due, then waits.
    pub(super) async fn release_notes_loop(self: Rc<Self>, phone: Option<String>) {
        loop {
            let next = self.db.setting(SETTING_NEXT_FETCH).await.ok().flatten().and_then(|v| v.parse::<i64>().ok());
            let wait = next.map_or(0, |next| (next - now_millis()).max(0));
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
            match self.fetch_release_notes(phone.as_deref()).await {
                Ok(()) => {
                    let next = now_millis() + FETCH_INTERVAL_MS;
                    let _ = self.db.set_setting(SETTING_NEXT_FETCH, &next.to_string()).await;
                }
                Err(error) => {
                    warn!(error = %format!("{error:#}"), "release notes: trying again later");
                    tokio::time::sleep(ERROR_RETRY).await;
                }
            }
        }
    }

    async fn fetch_release_notes(&self, phone: Option<&str>) -> anyhow::Result<()> {
        let client = release_notes::client()?;
        let current = release_notes::desktop_version(&client).await?;
        // The first time, start from now (Desktop starts from its version
        // when installed), so there's no backlog.
        let watermark = match self.db.setting(SETTING_WATERMARK).await?.and_then(|v| semver::Version::parse(&v).ok()) {
            Some(watermark) => watermark,
            None => {
                info!(%current, "release notes: starting from this Desktop version");
                self.db.set_setting(SETTING_WATERMARK, &current.to_string()).await?;
                current.clone()
            }
        };
        let hash = release_notes::manifest_hash(&client).await?.context("release notes manifest has no hash")?;
        let previous = self.db.setting(SETTING_MANIFEST_HASH).await?;
        let new_version = self.db.setting(SETTING_DESKTOP_VERSION).await?.as_deref() != Some(current.to_string().as_str());
        if previous.as_deref() == Some(hash.as_str()) && !new_version {
            debug!("release notes: manifest unchanged");
            return Ok(());
        }
        let manifest = release_notes::manifest(&client).await?;
        let due = release_notes::due(&manifest.announcements, &watermark, &current);
        if due.is_empty() {
            debug!("release notes: none new");
        } else {
            info!(count = due.len(), "release notes: adding");
            self.add_release_notes(&client, due, phone).await?;
        }
        self.db.set_setting(SETTING_MANIFEST_HASH, &hash).await?;
        self.db.set_setting(SETTING_DESKTOP_VERSION, &current.to_string()).await?;
        Ok(())
    }

    /// Desktop's `processReleaseNotes`: every note is fetched before any is
    /// added, so a failure adds none and they're tried again later.
    async fn add_release_notes(
        &self,
        client: &reqwest::Client,
        due: Vec<(semver::Version, release_notes::Announcement)>,
        phone: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some((newest, _)) = due.last().cloned() else { return Ok(()) };
        let chat = self.signal_chat().await?;
        if chat.blocked {
            info!("release notes: the Signal chat is blocked");
            self.db.set_setting(SETTING_WATERMARK, &newest.to_string()).await?;
            return Ok(());
        }
        let aci = self.self_aci();
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en".into());
        let locales = release_notes::locales(&locale);
        let mut notes = Vec::new();
        for (_, announcement) in due {
            if !release_notes::for_us(announcement.countries.as_deref(), phone, &aci) {
                debug!(uuid = announcement.uuid, "release note isn't for our country");
                continue;
            }
            let note = release_notes::note(client, &announcement.uuid, &locales).await?;
            let media = match &note.media {
                Some(path) => Some(release_notes::media(client, path).await?),
                None => None,
            };
            notes.push((announcement, note, media));
        }
        if notes.is_empty() {
            // As Desktop: nothing added, and the watermark stays.
            debug!("release notes: none for us");
            return Ok(());
        }
        let now = now_millis();
        for (ix, (announcement, note, media)) in notes.into_iter().enumerate() {
            let sent_at = now + ix as i64;
            let attachments = match media {
                Some((bytes, served_type)) => match self.save_release_media(&note, bytes, served_type).await {
                    Ok(meta) => vec![meta],
                    Err(error) => return Err(error.context("saving release note media")),
                },
                None => Vec::new(),
            };
            let (body, styles) = release_notes::compose(&note);
            let message = NewMessage {
                conversation_id: chat.id.clone(),
                sent_at,
                received_at: sent_at,
                direction: Direction::Incoming,
                author: Some(SIGNAL_ACI.into()),
                author_name: Some("Signal".into()),
                body,
                status: DeliveryStatus::Delivered,
                quote: None,
                attachments,
                link_preview: None,
                expires_in: 0,
            };
            let id = self.db.insert_message(&message).await?;
            self.db.set_styles(id, &styles).await?;
            if let Some(cta) = announcement.cta_id {
                self.db.set_cta(id, &CallToAction { id: cta, text: note.call_to_action_text.clone() }).await?;
            }
            self.emit(Event::MessageChanged { conversation: chat.id.clone(), message_id: id });
        }
        self.db.set_setting(SETTING_WATERMARK, &newest.to_string()).await?;
        self.emit(Event::ConversationsChanged);
        Ok(())
    }

    /// Desktop's `getOrCreateSignalConversation`: named Signal, and muted
    /// when made (unmuting it later sticks).
    async fn signal_chat(&self) -> anyhow::Result<signal_store::Conversation> {
        let id = ConversationId::signal();
        if let Some(chat) = self.db.conversation(&id).await? {
            return Ok(chat);
        }
        self.db
            .upsert_conversation(
                &id,
                ConversationUpdate { kind: Some(ConversationKind::Direct), title: Some("Signal".into()), ..Default::default() },
            )
            .await?;
        self.db.set_muted(&id, true).await?;
        self.db.conversation(&id).await?.context("the Signal chat wasn't saved")
    }

    /// Keeps a note's image or video with the attachments. An MP4 loops
    /// silently, as Desktop marks it a GIF.
    async fn save_release_media(
        &self,
        note: &release_notes::Note,
        bytes: Vec<u8>,
        served_type: Option<String>,
    ) -> anyhow::Result<AttachmentMeta> {
        let content_type = note
            .media_content_type
            .clone()
            .or(served_type)
            .context("release note media has no content type")?;
        let name = note.media.as_deref().and_then(|p| p.rsplit('/').next()).unwrap_or("media");
        let dir = self.config.data_dir.join("attachments").join(format!("release-note-{}", media::safe_file_name(&note.uuid)));
        let path = dir.join(media::safe_file_name(name));
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(&path, &bytes).await?;
        let (width, height) = note.media_size();
        Ok(AttachmentMeta {
            gif: content_type == "video/mp4",
            content_type,
            file_name: None,
            size: bytes.len() as u64,
            width,
            height,
            local_path: Some(path.to_string_lossy().into_owned()),
            sticker: false,
            voice: false,
            remote: None,
        })
    }
}
