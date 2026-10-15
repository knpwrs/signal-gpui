//! Release notes in the Signal chat, as Signal Desktop gets them
//! (`ts/services/releaseNoteAndMegaphoneFetcher.preload.ts`): a manifest on
//! updates2.signal.org lists announcements, each for Desktop versions from
//! `desktopMinVersion` and optionally for some countries; each note is a
//! JSON file per language, with an optional image or video.
//!
//! Desktop shows the notes for versions after the last one it showed, up to
//! its own version. This client has no Desktop version, so it uses the
//! current Desktop release's (from the updater's `latest-mac.yml`): the
//! notes appear when Desktop users get them.

use anyhow::{Context as _, bail};
use serde::Deserialize;
use signal_store::{StyleSpan, TextStyle};

/// Desktop's `resourcesUrl`.
const RESOURCES: &str = "https://updates2.signal.org";
const MANIFEST: &str = "/dynamic/release-notes/release-notes-v2.json";
const NOTES: &str = "/static/release-notes";
/// What Desktop's updater reads for the current macOS release.
const DESKTOP_RELEASE: &str = "/desktop/latest-mac.yml";
/// Desktop's `BUCKET_VALUE_HASH_SALT`.
const BUCKET_SALT: &str = "ReleaseNoteAndMegaphoneFetcher";
/// Between the title and the body (Desktop's `titleBodySeparator`).
const SEPARATOR: &str = "\n\n";
/// A note's image or video; Signal's are a few hundred KB.
const MAX_MEDIA_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub announcements: Vec<Announcement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    pub uuid: String,
    #[serde(default)]
    pub countries: Option<String>,
    #[serde(default)]
    pub desktop_min_version: Option<String>,
    #[serde(default)]
    pub cta_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub uuid: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub call_to_action_text: Option<String>,
    #[serde(default)]
    pub body_ranges: Vec<NoteRange>,
    /// A path on updates2.signal.org.
    #[serde(default)]
    pub media: Option<String>,
    /// Sizes come as strings ("720") or numbers.
    #[serde(default)]
    pub media_width: Option<serde_json::Value>,
    #[serde(default)]
    pub media_height: Option<serde_json::Value>,
    #[serde(default)]
    pub media_content_type: Option<String>,
}

/// A style over the note's body, in UTF-16 units (JavaScript's).
#[derive(Debug, Clone, Deserialize)]
pub struct NoteRange {
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub start: Option<usize>,
    #[serde(default)]
    pub length: Option<usize>,
}

impl Note {
    pub fn media_size(&self) -> (Option<u32>, Option<u32>) {
        let size = |v: &Option<serde_json::Value>| match v.as_ref()? {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
        .filter(|n| *n > 0);
        (size(&self.media_width), size(&self.media_height))
    }
}

/// The announcements due: for a Desktop version after `watermark` and up to
/// `current`, oldest first (Desktop's `validNotes`, sorted).
pub fn due(announcements: &[Announcement], watermark: &semver::Version, current: &semver::Version) -> Vec<(semver::Version, Announcement)> {
    let mut due: Vec<_> = announcements
        .iter()
        .filter_map(|a| {
            let version = semver::Version::parse(a.desktop_min_version.as_deref()?).ok()?;
            (&version > watermark && &version <= current).then(|| (version, a.clone()))
        })
        .collect();
    due.sort_by(|a, b| a.0.cmp(&b.0));
    due
}

/// Whether a note limited to some countries is for us (Desktop's
/// `isCountryPpmCsvBucketEnabled`): `countries` is `code:parts-per-million`
/// entries, `*` for everyone else, and our account's bucket (from its ACI)
/// must fall under the share for our number's country.
pub fn for_us(countries: Option<&str>, e164: Option<&str>, aci: &str) -> bool {
    let Some(countries) = countries.filter(|c| !c.is_empty()) else { return true };
    let Ok(aci) = uuid::Uuid::parse_str(aci) else { return false };
    let Some(share) = country_share(countries, e164) else { return false };
    bucket(&aci) < share
}

/// The share for our number's country. Country calling codes are
/// prefix-free, so the entry whose code starts the number is its country's.
/// With no number, only `*` applies.
fn country_share(countries: &str, e164: Option<&str>) -> Option<u64> {
    let digits = e164.map(|n| n.trim_start_matches('+'));
    let mut wildcard = None;
    for entry in countries.split(',') {
        let Some((code, value)) = entry.split_once(':') else { continue };
        let Ok(value) = value.trim().parse::<u64>() else { continue };
        let code = code.trim();
        if code == "*" {
            wildcard = Some(value);
        } else if !code.is_empty()
            && code.bytes().all(|b| b.is_ascii_digit())
            && digits.is_some_and(|d| d.starts_with(code))
        {
            return Some(value);
        }
    }
    wildcard
}

/// Our account's bucket, 0 to 999,999 (Desktop's `getBucketValue`).
fn bucket(aci: &uuid::Uuid) -> u64 {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update(format!("{BUCKET_SALT}.").as_bytes());
    hash.update(aci.as_bytes());
    let hash = hash.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash[..8]);
    u64::from_be_bytes(first) % 1_000_000
}

/// The languages to try, as Desktop does: the locale ("en_us"), its
/// language ("en"), then English.
pub fn locales(locale: &str) -> Vec<String> {
    // Desktop lowercases and replaces the first "-" only.
    let tag = locale.split(['.', '@']).next().unwrap_or(locale).replace('_', "-");
    let language = tag.split('-').next().unwrap_or("en").to_owned();
    let mut out = Vec::new();
    for candidate in [tag.to_lowercase().replacen('-', "_", 1), language.to_lowercase(), "en".into()] {
        if !candidate.is_empty() && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// The message for a note: its title in bold, a blank line, then the body
/// with its styles.
pub fn compose(note: &Note) -> (String, Vec<StyleSpan>) {
    let body = format!("{}{SEPARATOR}{}", note.title, note.body);
    let offset = note.title.len() + SEPARATOR.len();
    let mut styles = vec![StyleSpan { start: 0, len: note.title.len(), style: TextStyle::Bold }];
    let body_units = note.body.encode_utf16().count();
    for range in &note.body_ranges {
        let (Some(start), Some(length), Some(style)) = (range.start, range.length, range.style.as_deref()) else {
            continue;
        };
        // Desktop drops a range that runs past the body.
        if length == 0 || start + length > body_units {
            continue;
        }
        let style = match style {
            "bold" => TextStyle::Bold,
            "italic" => TextStyle::Italic,
            "strikethrough" => TextStyle::Strikethrough,
            "spoiler" => TextStyle::Spoiler,
            "mono" => TextStyle::Monospace,
            _ => continue,
        };
        let (Some(from), Some(to)) = (utf16_to_byte(&note.body, start), utf16_to_byte(&note.body, start + length)) else {
            continue;
        };
        styles.push(StyleSpan { start: offset + from, len: to - from, style });
    }
    styles.retain(|s| s.len > 0);
    (body, styles)
}

/// The byte offset of the `units`th UTF-16 unit, if it's on a character's
/// edge.
fn utf16_to_byte(text: &str, units: usize) -> Option<usize> {
    let mut seen = 0;
    for (at, c) in text.char_indices() {
        if seen == units {
            return Some(at);
        }
        seen += c.len_utf16();
    }
    (seen == units).then_some(text.len())
}

/// A client for updates2.signal.org, which has a certificate from Signal's
/// own root CA.
pub fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(crate::signal_tls()?)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// The current Desktop release's version.
pub async fn desktop_version(client: &reqwest::Client) -> anyhow::Result<semver::Version> {
    let text = client.get(format!("{RESOURCES}{DESKTOP_RELEASE}")).send().await?.error_for_status()?.text().await?;
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("version:"))
        .context("no version in Desktop's release file")?;
    Ok(semver::Version::parse(version.trim().trim_matches(['"', '\'']))?)
}

/// The manifest's ETag: it's only fetched when this changes.
pub async fn manifest_hash(client: &reqwest::Client) -> anyhow::Result<Option<String>> {
    let response = client.head(format!("{RESOURCES}{MANIFEST}")).send().await?.error_for_status()?;
    Ok(etag(&response))
}

pub async fn manifest(client: &reqwest::Client) -> anyhow::Result<Manifest> {
    let bytes = client.get(format!("{RESOURCES}{MANIFEST}")).send().await?.error_for_status()?.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// A note in the first of `locales` it's written in.
pub async fn note(client: &reqwest::Client, uuid: &str, locales: &[String]) -> anyhow::Result<Note> {
    for locale in locales {
        let url = format!("{RESOURCES}{NOTES}/{uuid}/{locale}.json");
        // Desktop asks whether the language exists before fetching it.
        let exists = match client.head(&url).send().await {
            Ok(response) => response.status().is_success() && etag(&response).is_some(),
            Err(_) => false,
        };
        if !exists {
            continue;
        }
        let fetched = async {
            let bytes = client.get(&url).send().await?.error_for_status()?.bytes().await?;
            anyhow::Ok(serde_json::from_slice::<Note>(&bytes)?)
        };
        match fetched.await {
            Ok(note) if note.uuid == uuid => return Ok(note),
            Ok(_) => tracing::warn!(uuid, locale, "release note's uuid doesn't match"),
            Err(error) => tracing::warn!(uuid, locale, error = %format!("{error:#}"), "couldn't fetch release note"),
        }
    }
    bail!("release note {uuid} isn't available in any of {locales:?}")
}

/// A note's image or video, and its type. Only from updates2.signal.org.
pub async fn media(client: &reqwest::Client, path: &str) -> anyhow::Result<(Vec<u8>, Option<String>)> {
    let url = media_url(path).context("release note media isn't on updates2.signal.org")?;
    let response = client.get(url).send().await?.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned());
    if response.content_length().is_some_and(|n| n as usize > MAX_MEDIA_BYTES) {
        bail!("release note media is too large");
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_MEDIA_BYTES {
        bail!("release note media is too large");
    }
    Ok((bytes.to_vec(), content_type))
}

/// Desktop's check: the media's URL must stay on the resources origin.
fn media_url(path: &str) -> Option<url::Url> {
    let base = url::Url::parse(RESOURCES).ok()?;
    let url = url::Url::parse(&format!("{RESOURCES}{path}")).ok()?;
    (url.origin() == base.origin()).then_some(url)
}

fn etag(response: &reqwest::Response) -> Option<String> {
    response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    fn announcement(uuid: &str, version: Option<&str>) -> Announcement {
        Announcement { uuid: uuid.into(), countries: None, desktop_min_version: version.map(Into::into), cta_id: None }
    }

    #[test]
    fn notes_are_due_after_the_watermark_up_to_the_current_version() {
        let all = [
            announcement("phones-only", None),
            announcement("old", Some("8.30.0")),
            announcement("newest", Some("8.32.0")),
            announcement("now", Some("8.31.0")),
            announcement("future", Some("8.33.0")),
            announcement("broken", Some("eight")),
        ];
        let due: Vec<_> = due(&all, &v("8.30.0"), &v("8.32.0")).into_iter().map(|(_, a)| a.uuid).collect();
        assert_eq!(due, ["now", "newest"], "oldest first");
    }

    #[test]
    fn country_shares_follow_desktops_buckets() {
        let aci = "11111111-1111-4111-8111-111111111111";
        let bucket = bucket(&uuid::Uuid::parse_str(aci).unwrap());
        assert!(bucket < 1_000_000);
        assert!(for_us(None, None, aci), "no countries: everyone");
        assert!(for_us(Some("1:1000000,44:1000000"), Some("+15551234567"), aci));
        assert!(!for_us(Some("44:1000000"), Some("+15551234567"), aci), "another country");
        assert!(!for_us(Some("1:0"), Some("+15551234567"), aci), "none of ours");
        assert!(for_us(Some("44:0,*:1000000"), Some("+15551234567"), aci), "everyone else");
        assert!(!for_us(Some("1:1000000"), None, aci), "no number: only *");
        // Exactly at our bucket: not yet.
        let at = format!("1:{bucket}");
        assert!(!for_us(Some(&at), Some("+15551234567"), aci));
        let above = format!("1:{}", bucket + 1);
        assert!(for_us(Some(&above), Some("+15551234567"), aci));
        assert!(!for_us(Some("1:1000000"), Some("+15551234567"), "not an aci"));
    }

    #[test]
    fn languages_are_tried_as_desktop_does() {
        assert_eq!(locales("en-US"), ["en_us", "en"]);
        assert_eq!(locales("de_DE.UTF-8"), ["de_de", "de", "en"]);
        assert_eq!(locales("zh-Hans-CN"), ["zh_hans-cn", "zh", "en"]);
        assert_eq!(locales("en"), ["en"]);
    }

    #[test]
    fn a_note_is_its_bold_title_then_its_styled_body() {
        let note = Note {
            uuid: "u".into(),
            title: "Hello".into(),
            body: "😀 big news".into(),
            call_to_action_text: None,
            body_ranges: vec![
                // "big", after the emoji's two UTF-16 units and a space.
                NoteRange { style: Some("bold".into()), start: Some(3), length: Some(3) },
                NoteRange { style: Some("mono".into()), start: Some(7), length: Some(4) },
                // Past the end, unknown, or inside the emoji: dropped.
                NoteRange { style: Some("italic".into()), start: Some(8), length: Some(9) },
                NoteRange { style: Some("glow".into()), start: Some(0), length: Some(1) },
                NoteRange { style: Some("italic".into()), start: Some(1), length: Some(2) },
            ],
            media: None,
            media_width: Some(serde_json::json!("1080")),
            media_height: Some(serde_json::json!(720)),
            media_content_type: None,
        };
        let (body, styles) = compose(&note);
        assert_eq!(body, "Hello\n\n😀 big news");
        let parts: Vec<_> = styles.iter().map(|s| (&body[s.start..s.start + s.len], s.style)).collect();
        assert_eq!(parts, [("Hello", TextStyle::Bold), ("big", TextStyle::Bold), ("news", TextStyle::Monospace)]);
        assert_eq!(note.media_size(), (Some(1080), Some(720)));
    }

    #[test]
    fn media_stays_on_signals_server() {
        assert!(media_url("/static/release-notes/water-plant.mp4").is_some());
        assert!(media_url("@evil.example/x.mp4").is_none());
        assert!(media_url(".evil.example/x.mp4").is_none());
    }

    /// Against Signal's server (no account involved): `cargo test -p
    /// signal-backend live_release_notes -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_release_notes() {
        let client = client().unwrap();
        let current = desktop_version(&client).await.unwrap();
        assert!(manifest_hash(&client).await.unwrap().is_some());
        let manifest = manifest(&client).await.unwrap();
        let older = semver::Version::new(current.major.saturating_sub(1), 0, 0);
        for (_, announcement) in due(&manifest.announcements, &older, &current) {
            let note = note(&client, &announcement.uuid, &locales("en-US")).await.unwrap();
            let (body, styles) = compose(&note);
            assert!(body.starts_with(&note.title) && !styles.is_empty());
            if let Some(path) = &note.media {
                let (bytes, kind) = media(&client, path).await.unwrap();
                assert!(!bytes.is_empty() && kind.is_some());
            }
        }
    }

    #[test]
    fn signals_manifest_parses() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"announcements":[{"uuid":"a","iosMinVersion":"8.17.0.0"},
                {"uuid":"b","ctaId":"donate","countries":"1:1000000","desktopMinVersion":"8.31.0","includeBoostMessage":false}],
                "megaphones":[]}"#,
        )
        .unwrap();
        assert_eq!(manifest.announcements.len(), 2);
        assert_eq!(manifest.announcements[1].cta_id.as_deref(), Some("donate"));
    }
}
