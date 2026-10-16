//! Desktop's media auto-download (Data usage, `auto-download-attachment`):
//! which kinds of attachment download as they arrive. Like Desktop
//! (`ts/util/queueAttachmentDownloads.preload.ts`), a video (a GIF too) is
//! a video, an image a photo, a voice message under 100 KiB and a sticker
//! always download, other audio is audio, the rest documents; and nothing
//! at or over the remote config's limit downloads on its own.

use std::sync::{Arc, RwLock};

use signal_store::AttachmentMeta;

/// Settings key: the choices, as JSON.
pub const SETTING_AUTO_DOWNLOAD: &str = "auto_download";

/// Desktop's `MAX_VOICE_MESSAGE_AUTO_DOWNLOAD_SIZE`.
const MAX_VOICE_BYTES: u64 = 100 * 1024;

/// Desktop's `AutoDownloadAttachmentType`; all on by default
/// (`DEFAULT_AUTO_DOWNLOAD_ATTACHMENT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutoDownload {
    #[serde(default = "on")]
    pub photos: bool,
    #[serde(default = "on")]
    pub videos: bool,
    #[serde(default = "on")]
    pub audio: bool,
    #[serde(default = "on")]
    pub documents: bool,
}

fn on() -> bool {
    true
}

impl Default for AutoDownload {
    fn default() -> Self {
        Self { photos: true, videos: true, audio: true, documents: true }
    }
}

impl AutoDownload {
    /// From the stored JSON (Desktop's defaults when there's none).
    pub fn from_setting(value: Option<&str>) -> Self {
        value.and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default()
    }

    /// Whether `attachment` downloads as it arrives.
    pub fn downloads(&self, attachment: &AttachmentMeta) -> bool {
        let kind = attachment.content_type.as_str();
        let chosen = if attachment.sticker || (attachment.voice && attachment.size < MAX_VOICE_BYTES) {
            true
        } else if kind.starts_with("video/") || attachment.gif {
            self.videos
        } else if kind.starts_with("image/") {
            self.photos
        } else if kind.starts_with("audio/") || attachment.voice {
            self.audio
        } else {
            self.documents
        };
        chosen && crate::remote_config().auto_downloads(attachment.size)
    }
}

static CURRENT: RwLock<Option<Arc<AutoDownload>>> = RwLock::new(None);

/// The choices in effect.
pub fn current() -> AutoDownload {
    CURRENT.read().ok().and_then(|c| c.as_deref().copied()).unwrap_or_default()
}

pub(crate) fn set_current(choices: AutoDownload) {
    if let Ok(mut current) = CURRENT.write() {
        *current = Some(Arc::new(choices));
    }
}

/// Puts the saved choices in effect.
pub(crate) async fn load(db: &signal_store::AppDb) {
    let value = db.setting(SETTING_AUTO_DOWNLOAD).await.ok().flatten();
    set_current(AutoDownload::from_setting(value.as_deref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(content_type: &str, size: u64) -> AttachmentMeta {
        serde_json::from_value(serde_json::json!({
            "content_type": content_type, "file_name": null, "size": size, "width": null, "height": null, "local_path": null
        }))
        .unwrap()
    }

    #[test]
    fn kinds_follow_desktops_choices() {
        let all = AutoDownload::default();
        for kind in ["image/jpeg", "video/mp4", "audio/aac", "application/pdf"] {
            assert!(all.downloads(&meta(kind, 1000)), "{kind}");
        }
        let none = AutoDownload { photos: false, videos: false, audio: false, documents: false };
        for kind in ["image/jpeg", "video/mp4", "audio/aac", "application/pdf"] {
            assert!(!none.downloads(&meta(kind, 1000)), "{kind}");
        }
        // A GIF is a video; a short voice message and a sticker always come.
        let gif = AttachmentMeta { gif: true, ..meta("video/mp4", 1000) };
        assert!(!AutoDownload { videos: false, ..all }.downloads(&gif));
        assert!(AutoDownload { photos: false, ..all }.downloads(&gif));
        let voice = AttachmentMeta { voice: true, ..meta("audio/aac", 50_000) };
        assert!(none.downloads(&voice));
        let long_voice = AttachmentMeta { voice: true, ..meta("audio/aac", 500_000) };
        assert!(!none.downloads(&long_voice) && all.downloads(&long_voice));
        let sticker = AttachmentMeta { sticker: true, ..meta("image/webp", 20_000) };
        assert!(none.downloads(&sticker));
        // Over the remote config's limit, nothing downloads on its own.
        assert!(!all.downloads(&meta("image/jpeg", crate::remote_config().auto_download_max_bytes())));
        // Stored choices read back, a missing one on.
        let only = AutoDownload::from_setting(Some(r#"{"photos":false}"#));
        assert_eq!(only, AutoDownload { photos: false, ..all });
        assert_eq!(AutoDownload::from_setting(None), all);
    }
}
