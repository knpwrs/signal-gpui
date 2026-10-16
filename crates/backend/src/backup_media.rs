//! Attachments in the backup (media) tier, as Signal Desktop downloads them
//! (`ts/util/downloadAttachment.preload.ts`,
//! `ts/textsecure/downloadAttachment.preload.ts`, `ts/services/backups`).
//!
//! A transferred backup can name an attachment that's only in the account's
//! backup tier: its locator has the attachment's key and plaintext hash but
//! no transit-tier key. The file is in the backup CDN under a media id
//! derived from the media root backup key, encrypted twice: the attachment's
//! own encryption, then a layer keyed by the media id. Reading it takes a
//! backup auth credential (zkgroup), presented and signed with a key derived
//! from the media root key, for the CDN's read headers and the backup's
//! directories.
//!
//! Desktop only tries this with paid media backups (`hasMediaBackups`:
//! `backupTier` from the account's storage record, `BackupLevel.Paid`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use backup_account_keys::BackupKey;
use libsignal_core::Aci;
use libsignal_core::curve::PrivateKey;
use presage::libsignal_service::configuration::{Endpoint, SignalServers};
use presage::libsignal_service::push_service::HttpAuthOverride;
use signal_store::{AppDb, RemoteAttachment};
use zkgroup::backups::{
    BackupAuthCredential, BackupAuthCredentialRequestContext, BackupAuthCredentialResponse, BackupLevel,
};
use zkgroup::generic_server_params::GenericServerPublicParams;

/// The media root backup key, hex: from linking, or a keys sync from the
/// phone (Desktop's `backupMediaRootKey`).
pub(crate) const SETTING_MEDIA_ROOT_KEY: &str = "backup.media_root_key";
/// The account record's `backupTier` from the Storage Service (Desktop's
/// `backupTier`): a `BackupLevel`, `0` when backups are off, absent until
/// the record has been read.
pub(crate) const SETTING_BACKUP_TIER: &str = "backup.tier";
/// The media signing key's public half (base64) last uploaded (Desktop's
/// `setBackupMediaSignatureKey`).
pub(crate) const SETTING_SIGNATURE_KEY_SET: &str = "backup.media_signature_key_set";
/// When we last asked the phone for its keys (Unix millis), so it's asked
/// at most daily (Desktop's `accountEntropyPoolLastRequestTime`).
pub(crate) const SETTING_KEYS_REQUESTED: &str = "backup.keys_requested_at";

/// Desktop's `backupServerPublicParams` (`config/production.json`).
const PRODUCTION_PARAMS: &str = "AZwNSU55fsFCbgaxGRD11wO1juAs8Yr5GF8FPlGzzvdJJIKH5/4CC7ZJSOe3yL2vturVaRU2Cx0n751Vt8wkj1Y4pyiScu0/S10n647ipo+iq97JZQv+UOlwH8ThyNlGT5DfxXCwTqivxHuXvZpuezPgHk5Gxl5aC6xuNxOnwmFlmu4CeSgdhW8+Pp0vAJOQ1MsU2D0+/kzI+tU94nB3tybY/Ao1AcGW2q41uKQbnOJUWwmQaFT6s+xTISgzsg7CPox6oORGX8rnyk/9lic3DbGsUHctIVpMAl/ogJBb4aYC";
/// And for staging (`config/default.json`).
const STAGING_PARAMS: &str = "AXYrGb9IfugAAJiPKp+mdXUx+OL9zBolPYHYQz6GI1gWjpEu5me3zVNSvmYY4zWboZHif+HG1sDHSuvwFd0QszS6h3nZ6vRdM/IYGK+cLynw3ucWo7idf3zjOG3b6JnGT/z7XYCr6HuOGkWH4DQWCH98hxVZMGOgmT8DCQoqebQb3oK1yrwEglRWmtI01KhRg9RGUKoQiwuej1JZEY8uaG4Uz9n1cVODJ1iuByhNqGHo+KfI4iWhjtx2AnhYqHViQ3CMd4ASGBJtic9UTFVk/4vegVIy0wfYsAmViftzK6t4";

/// Desktop's `MAX_BACKUP_THUMBNAIL_SIZE`.
const MAX_THUMBNAIL_BYTES: u64 = 8 * 1024;

/// CDN read credentials are good for a day; Desktop keeps them 12 hours
/// (`BACKUP_CDN_READ_CREDENTIALS_VALID_DURATION`).
const CDN_HEADERS_FOR: Duration = Duration::from_secs(12 * 60 * 60);

/// After the server says there's no backup (or no paid one), don't ask
/// again for this long.
const UNAVAILABLE_FOR: Duration = Duration::from_secs(60 * 60);

/// After any other failure to get credentials or details, wait this long.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

const DAY_SECONDS: u64 = 24 * 60 * 60;

/// Desktop's `global.backups.mediaTierFallbackCdnNumber` remote config key:
/// the backup CDN for an attachment whose backup didn't name one.
const FALLBACK_CDN_CONFIG: &str = "global.backups.mediaTierFallbackCdnNumber";

pub(crate) fn server_params(servers: SignalServers) -> anyhow::Result<GenericServerPublicParams> {
    let encoded = match servers {
        SignalServers::Production => PRODUCTION_PARAMS,
        SignalServers::Staging => STAGING_PARAMS,
    };
    GenericServerPublicParams::try_from(STANDARD.decode(encoded)?.as_slice()).map_err(|_| anyhow!("bad backup server params"))
}

/// Desktop's `getMediaName`: the plaintext hash, then the attachment key,
/// in lowercase hex.
pub(crate) fn media_name(plaintext_hash: &[u8], key: &[u8]) -> String {
    let mut name = hex::encode(plaintext_hash);
    name.push_str(&hex::encode(key));
    name
}

/// Desktop's `getMediaNameForAttachmentThumbnail`.
pub(crate) fn thumbnail_media_name(name: &str) -> String {
    format!("{name}_thumbnail")
}

/// A media id: what the backup tier calls a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaId(pub [u8; backup_account_keys::MEDIA_ID_LEN]);

impl MediaId {
    /// Desktop's `getMediaIdFromMediaName`.
    pub(crate) fn new(root: &BackupKey, media_name: &str) -> Self {
        Self(root.derive_media_id(media_name))
    }

    /// As in URLs (Desktop's `Bytes.toBase64url`).
    pub(crate) fn string(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

/// Desktop's derived key material is the MAC key, then the AES key
/// (`deriveBackupMediaKeyMaterial`); attachment ciphers take AES first.
fn aes_then_mac(material: [u8; 64]) -> [u8; 64] {
    let mut key = [0; 64];
    key[..32].copy_from_slice(&material[32..]);
    key[32..].copy_from_slice(&material[..32]);
    key
}

/// The outer layer's key for a file in the backup tier
/// (`deriveBackupMediaKeyMaterial`).
pub(crate) fn outer_key(root: &BackupKey, id: &MediaId) -> [u8; 64] {
    aes_then_mac(root.derive_media_encryption_key_data(&id.0))
}

/// A backup thumbnail's own (inner) key
/// (`deriveBackupThumbnailTransitKeyMaterial`).
pub(crate) fn thumbnail_inner_key(root: &BackupKey, id: &MediaId) -> [u8; 64] {
    aes_then_mac(root.derive_thumbnail_transit_encryption_key_data(&id.0))
}

/// Desktop's `getAttachmentCiphertextSize` for the backup tier: the transit
/// tier's size, encrypted again (and padded again in the estimate).
pub(crate) fn ciphertext_size(plaintext: u64) -> u64 {
    let inner = crate::long_text::ciphertext_size(plaintext);
    let padded = crate::long_text::log_pad(inner);
    16 + (padded / 16 + 1) * 16 + 32
}

/// What the download may come to: Desktop's `checkSize` allows 5% over the
/// expected size. A thumbnail may be up to twice its usual most.
pub(crate) fn download_limit(size: u64, thumbnail: bool) -> u64 {
    let expected = if thumbnail { ciphertext_size(MAX_THUMBNAIL_BYTES * 2) } else { ciphertext_size(size) };
    expected + expected / 20
}

fn decrypt_layer(key: [u8; 64], data: &mut Vec<u8>, what: &str) -> anyhow::Result<()> {
    if data.len() < 48 {
        bail!("the {what} is too short");
    }
    presage::libsignal_service::attachment_cipher::decrypt_in_place(key, data)
        .map_err(|e| anyhow!("can't decrypt the {what}: {e:?}"))
}

/// Takes off both layers of a full-size file from the backup tier and
/// checks it against its plaintext hash (Desktop's `decryptAndReencryptLocally`
/// with `outerEncryption`). `size` trims the padding.
pub(crate) fn decrypt_media(root: &BackupKey, remote: &RemoteAttachment, size: u64, mut data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    use sha2::Digest as _;
    let (hash, key) = media_parts(remote).context("not in the backup tier")?;
    let id = MediaId::new(root, &media_name(&hash, &key));
    decrypt_layer(outer_key(root, &id), &mut data, "backup layer")?;
    decrypt_layer(key, &mut data, "attachment")?;
    if size > 0 && (size as usize) < data.len() {
        data.truncate(size as usize);
    }
    if sha2::Sha256::digest(&data).as_slice() != hash.as_slice() {
        bail!("the download doesn't match its hash");
    }
    Ok(data)
}

/// Takes off both layers of a backup thumbnail. Desktop made it, so there's
/// no hash to check (its `backupThumbnail` decrypt type), and it isn't
/// padded beyond the cipher's own padding.
pub(crate) fn decrypt_thumbnail(root: &BackupKey, remote: &RemoteAttachment, mut data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let (hash, key) = media_parts(remote).context("not in the backup tier")?;
    let id = MediaId::new(root, &thumbnail_media_name(&media_name(&hash, &key)));
    decrypt_layer(outer_key(root, &id), &mut data, "backup layer")?;
    decrypt_layer(thumbnail_inner_key(root, &id), &mut data, "thumbnail")?;
    Ok(data)
}

/// The plaintext hash and key that name a file in the backup tier, when the
/// locator has both (Desktop's `hasRequiredInformationForRemoteBackup`).
pub(crate) fn media_parts(remote: &RemoteAttachment) -> Option<(Vec<u8>, [u8; 64])> {
    let hash = hex::decode(remote.plaintext_hash.as_deref()?).ok().filter(|h| h.len() == 32)?;
    let key: [u8; 64] = hex::decode(&remote.key).ok()?.try_into().ok()?;
    Some((hash, key))
}

/// Desktop's `canAttachmentHaveThumbnail`: images and videos.
pub(crate) fn can_have_thumbnail(content_type: &str) -> bool {
    content_type.starts_with("image/") || content_type.starts_with("video/")
}

// ------------------------------------------------------------------ requests

/// `GET /v1/archives/auth`: a week of credentials from today, by UTC day
/// (Desktop's `getBackupCredentials`, from `toDayMillis(now)` to
/// `toDayMillis(now + 6 days)`).
pub(crate) fn credentials_path(now_seconds: u64) -> (u64, u64, String) {
    let start = now_seconds - now_seconds % DAY_SECONDS;
    let end = start + 6 * DAY_SECONDS;
    (start, end, format!("/v1/archives/auth?redemptionStartSeconds={start}&redemptionEndSeconds={end}"))
}

/// Reads the media credentials out of `GET /v1/archives/auth`'s answer,
/// checking them as Desktop's `BackupCredentials#doFetch` does: each in the
/// asked range, none twice.
pub(crate) fn receive_credentials(
    json: &[u8],
    context: &BackupAuthCredentialRequestContext,
    params: &GenericServerPublicParams,
    start: u64,
    end: u64,
) -> anyhow::Result<Vec<(u64, BackupAuthCredential)>> {
    #[derive(serde::Deserialize)]
    struct Answer {
        credentials: Lists,
    }
    #[derive(serde::Deserialize)]
    struct Lists {
        media: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        credential: String,
        redemption_time: u64,
    }
    let answer: Answer = serde_json::from_slice(json).context("unexpected credentials answer")?;
    let mut credentials: Vec<(u64, BackupAuthCredential)> = Vec::new();
    for entry in answer.credentials.media {
        let at = entry.redemption_time;
        if at < start || at > end {
            bail!("a backup credential is for a day that wasn't asked for");
        }
        if credentials.iter().any(|(day, _)| *day == at) {
            bail!("two backup credentials for one day");
        }
        let response: BackupAuthCredentialResponse =
            zkgroup::deserialize(&STANDARD.decode(&entry.credential)?).map_err(|_| anyhow!("bad backup credential"))?;
        let credential = context
            .clone()
            .receive(response, params, zkgroup::Timestamp::from_epoch_seconds(at))
            .map_err(|_| anyhow!("backup credential didn't verify"))?;
        credentials.push((at, credential));
    }
    Ok(credentials)
}

/// A credential presented for one request: the presentation and its
/// signature by the backup's signing key, in the order libsignal makes them
/// (`rust/net/chat/src/api/backups.rs`, `BackupAuth::present`).
pub(crate) fn present(
    credential: &BackupAuthCredential,
    params: &GenericServerPublicParams,
    signing_key: &PrivateKey,
    rng: &mut (impl rand::CryptoRng + ?Sized),
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    use rand::Rng as _;
    let presentation = credential.present(params, rng.random());
    let presentation = zkgroup::serialize(&presentation);
    let signature = signing_key.calculate_signature(&presentation, rng).map_err(|e| anyhow!("can't sign: {e}"))?;
    Ok((presentation, signature.into()))
}

/// The headers that carry a presentation (libsignal's
/// `BackupAuthPresentation::to_headers`).
pub(crate) fn auth_headers(presentation: &[u8], signature: &[u8]) -> [(&'static str, String); 2] {
    [("x-signal-zk-auth", STANDARD.encode(presentation)), ("x-signal-zk-auth-signature", STANDARD.encode(signature))]
}

/// `PUT /v1/archives/keys`'s body: the signing key's public half, as
/// libsignal serializes it (with its type byte).
pub(crate) fn set_public_key_body(signing_key: &PrivateKey) -> anyhow::Result<serde_json::Value> {
    let public = signing_key.public_key().map_err(|e| anyhow!("{e}"))?;
    Ok(serde_json::json!({ "backupIdPublicKey": STANDARD.encode(public.serialize()) }))
}

/// `GET /v1/archives/auth/read`: headers for reading one backup CDN
/// (Desktop's `getBackupCDNCredentials`).
pub(crate) fn read_credentials_path(cdn: u32) -> String {
    format!("/v1/archives/auth/read?cdn={cdn}")
}

pub(crate) fn parse_read_credentials(json: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    #[derive(serde::Deserialize)]
    struct Answer {
        headers: HashMap<String, String>,
    }
    let answer: Answer = serde_json::from_slice(json).context("unexpected CDN credentials answer")?;
    let mut headers: Vec<(String, String)> = answer.headers.into_iter().collect();
    headers.sort();
    Ok(headers)
}

/// `GET /v1/archives`: the backup's directories on the CDN (Desktop's
/// `getMediaBackupInfo`, with a media credential).
pub(crate) fn parse_backup_info(json: &[u8]) -> anyhow::Result<(String, String)> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Answer {
        backup_dir: String,
        media_dir: String,
    }
    let answer: Answer = serde_json::from_slice(json).context("unexpected backup info")?;
    Ok((answer.backup_dir, answer.media_dir))
}

/// Where a file is on the backup CDN (Desktop's
/// `getAttachmentFromBackupTier`: `backups/{backupDir}/{mediaDir}/{mediaId}`).
/// The server's names are base64url; anything else is refused rather than
/// escaped.
pub(crate) fn media_path(backup_dir: &str, media_dir: &str, id: &MediaId) -> anyhow::Result<String> {
    let safe = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b));
    if !safe(backup_dir) || !safe(media_dir) {
        bail!("unexpected backup directory names");
    }
    Ok(format!("backups/{backup_dir}/{media_dir}/{}", id.string()))
}

/// The backup CDN for a file whose backup didn't name one (Desktop's
/// `getCdnNumberForBackupTier`, from remote config).
pub(crate) fn fallback_cdn(config: &crate::remote_config::RemoteConfig) -> Option<u32> {
    config.int(FALLBACK_CDN_CONFIG).and_then(|cdn| u32::try_from(cdn).ok())
}

// ------------------------------------------------------------------- session

/// What one run knows about the backup tier, kept in memory as Desktop
/// keeps its CDN credentials (`#cachedCdnReadCredentials`) and backup info.
#[derive(Default)]
struct Session {
    /// For this root key (a key sync can change it).
    root: Option<[u8; 32]>,
    /// Media credentials by redemption day (Unix seconds).
    credentials: Vec<(u64, BackupAuthCredential)>,
    /// `(backupDir, mediaDir)`.
    dirs: Option<(String, String)>,
    /// Read headers by CDN, with when they were fetched.
    cdn_headers: HashMap<u32, (Instant, Vec<(String, String)>)>,
    /// Until when not to try: the server said there's no (paid) backup.
    unavailable: Option<(Instant, String)>,
}

/// Downloads from the backup tier, one credential fetch at a time.
#[derive(Default)]
pub(crate) struct BackupMedia {
    session: tokio::sync::Mutex<Session>,
}

/// What's needed to read one file from the backup tier.
struct Access {
    cdn: u32,
    headers: Vec<(String, String)>,
    backup_dir: String,
    media_dir: String,
}

impl BackupMedia {
    /// The media root key, when there is one and the account's backup tier
    /// doesn't rule the backup tier out: Desktop's `hasMediaBackups` gate.
    /// An unknown tier (the storage record not read yet) leaves it to the
    /// credential's level.
    pub(crate) async fn root_key(db: &AppDb) -> Option<BackupKey> {
        let tier = db.setting(SETTING_BACKUP_TIER).await.ok().flatten();
        if tier.is_some_and(|t| t != u64::from(BackupLevel::Paid).to_string()) {
            return None;
        }
        let hex_key = db.setting(SETTING_MEDIA_ROOT_KEY).await.ok().flatten()?;
        let key: [u8; 32] = hex::decode(hex_key).ok()?.try_into().ok()?;
        Some(BackupKey(key))
    }

    /// Forgets what this run knew (the root key or tier changed).
    pub(crate) async fn reset(&self) {
        *self.session.lock().await = Session::default();
    }

    /// Downloads and decrypts a full-size file from the backup tier.
    pub(crate) async fn download(
        &self,
        manager: &crate::actor::SignalManager,
        db: &AppDb,
        root: &BackupKey,
        remote: &RemoteAttachment,
        size: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let (hash, key) = media_parts(remote).context("not in the backup tier")?;
        let id = MediaId::new(root, &media_name(&hash, &key));
        let data = self.fetch(manager, db, root, remote.media_tier_cdn, &id, download_limit(size, false)).await?;
        let (root, remote) = (BackupKey(root.0), remote.clone());
        tokio::task::spawn_blocking(move || decrypt_media(&root, &remote, size, data)).await?
    }

    /// Downloads and decrypts a file's thumbnail from the backup tier
    /// (Desktop's `downloadBackupThumbnail`): a WebP.
    pub(crate) async fn download_thumbnail(
        &self,
        manager: &crate::actor::SignalManager,
        db: &AppDb,
        root: &BackupKey,
        remote: &RemoteAttachment,
    ) -> anyhow::Result<Vec<u8>> {
        let (hash, key) = media_parts(remote).context("not in the backup tier")?;
        let id = MediaId::new(root, &thumbnail_media_name(&media_name(&hash, &key)));
        let data = self.fetch(manager, db, root, remote.media_tier_cdn, &id, download_limit(0, true)).await?;
        let (root, remote) = (BackupKey(root.0), remote.clone());
        tokio::task::spawn_blocking(move || decrypt_thumbnail(&root, &remote, data)).await?
    }

    async fn fetch(
        &self,
        manager: &crate::actor::SignalManager,
        db: &AppDb,
        root: &BackupKey,
        cdn: Option<u32>,
        id: &MediaId,
        limit: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let access = self.access(manager, db, root, cdn).await?;
        let service = crate::storage::push_service(manager)?;
        let path = media_path(&access.backup_dir, &access.media_dir, id)?;
        let mut request = service.request(reqwest::Method::GET, Endpoint::cdn(access.cdn, path), HttpAuthOverride::Unidentified)?;
        for (name, value) in &access.headers {
            request = request.header(name, value);
        }
        let mut response = request.send().await?;
        match response.status().as_u16() {
            200 => {}
            // Desktop's `onCdnCredentialError`: fetch new ones next time.
            401 | 403 => {
                self.session.lock().await.cdn_headers.remove(&access.cdn);
                bail!("the backup CDN refused its credentials");
            }
            404 => bail!("it isn't in your backup"),
            status => bail!("the backup CDN answered {status}"),
        }
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            data.extend_from_slice(&chunk);
            if data.len() as u64 > limit {
                bail!("the download is bigger than it should be");
            }
        }
        Ok(data)
    }

    /// Credentials, the CDN's read headers and the backup's directories,
    /// fetched as needed and kept for the run.
    async fn access(&self, manager: &crate::actor::SignalManager, db: &AppDb, root: &BackupKey, cdn: Option<u32>) -> anyhow::Result<Access> {
        let mut session = self.session.lock().await;
        if session.root != Some(root.0) {
            *session = Session { root: Some(root.0), ..Session::default() };
        }
        if let Some((until, why)) = &session.unavailable {
            if Instant::now() < *until {
                bail!("{why}");
            }
            session.unavailable = None;
        }
        let result = Self::prepare(&mut session, manager, db, root, cdn).await;
        // Any other failure (offline, a server error) also holds off the
        // next attachments for a while rather than each asking again.
        if let Err(error) = &result
            && session.unavailable.is_none()
        {
            session.unavailable = Some((Instant::now() + RETRY_AFTER, format!("{error:#}")));
        }
        result?.context("the backup didn't say which CDN it's on")
    }

    /// `None`: the file's CDN isn't known (the backup didn't name one and
    /// remote config has no fallback).
    async fn prepare(
        session: &mut Session,
        manager: &crate::actor::SignalManager,
        db: &AppDb,
        root: &BackupKey,
        cdn: Option<u32>,
    ) -> anyhow::Result<Option<Access>> {
        let service = crate::storage::push_service(manager)?;
        let cdn = match cdn {
            Some(cdn) => cdn,
            None => {
                match fallback_cdn(&crate::remote_config()) {
                    Some(cdn) => cdn,
                    // Only this file's problem: not held against the rest.
                    None => return Ok(None),
                }
            }
        };
        let data = manager.registration_data();
        let params = server_params(data.signal_servers)?;
        let aci = Aci::from(uuid::Uuid::from(data.service_ids.aci()));
        let signing_key = root.derive_ec_key(&aci);

        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let today = now - now % DAY_SECONDS;
        if !session.credentials.iter().any(|(day, _)| *day == today) {
            match fetch_credentials(&service, root, aci, &params, now).await? {
                Some(credentials) => session.credentials = credentials,
                None => {
                    let why = "there's no backup for this account".to_owned();
                    session.unavailable = Some((Instant::now() + UNAVAILABLE_FOR, why.clone()));
                    bail!("{why}");
                }
            }
        }
        let credential =
            session.credentials.iter().find(|(day, _)| *day == today).map(|(_, c)| c.clone()).context("no backup credential for today")?;
        if credential.backup_level() != BackupLevel::Paid {
            let why = "media backups aren't on for this account".to_owned();
            session.unavailable = Some((Instant::now() + UNAVAILABLE_FOR, why.clone()));
            bail!("{why}");
        }
        let headers = |rng: &mut rand::rngs::ThreadRng| -> anyhow::Result<[(&'static str, String); 2]> {
            let (presentation, signature) = present(&credential, &params, &signing_key, rng)?;
            Ok(auth_headers(&presentation, &signature))
        };

        // Desktop's `getForToday`: the signing key goes up once. The setting
        // holds the public key uploaded, so a key derived from another root
        // key or ACI goes up again.
        let body = set_public_key_body(&signing_key)?;
        let public = body["backupIdPublicKey"].as_str().unwrap_or_default().to_owned();
        if db.setting(SETTING_SIGNATURE_KEY_SET).await.ok().flatten().as_deref() != Some(public.as_str()) {
            let mut request = service.request(reqwest::Method::PUT, Endpoint::service("/v1/archives/keys"), HttpAuthOverride::Unidentified)?;
            for (name, value) in headers(&mut rand::rng())? {
                request = request.header(name, value);
            }
            request.json(&body).send().await?.error_for_status().context("uploading the backup signing key")?;
            db.set_setting(SETTING_SIGNATURE_KEY_SET, &public).await?;
        }

        if session.dirs.is_none() {
            let mut request = service.request(reqwest::Method::GET, Endpoint::service("/v1/archives"), HttpAuthOverride::Unidentified)?;
            for (name, value) in headers(&mut rand::rng())? {
                request = request.header(name, value);
            }
            let body = request.send().await?.error_for_status().context("reading the backup's details")?.bytes().await?;
            session.dirs = Some(parse_backup_info(&body)?);
        }

        let fresh = session.cdn_headers.get(&cdn).is_some_and(|(at, _)| at.elapsed() < CDN_HEADERS_FOR);
        if !fresh {
            let mut request =
                service.request(reqwest::Method::GET, Endpoint::service(read_credentials_path(cdn)), HttpAuthOverride::Unidentified)?;
            for (name, value) in headers(&mut rand::rng())? {
                request = request.header(name, value);
            }
            let body = request.send().await?.error_for_status().context("getting backup CDN credentials")?.bytes().await?;
            session.cdn_headers.insert(cdn, (Instant::now(), parse_read_credentials(&body)?));
        }
        let (backup_dir, media_dir) = session.dirs.clone().context("no backup details")?;
        let headers = session.cdn_headers.get(&cdn).map(|(_, h)| h.clone()).unwrap_or_default();
        Ok(Some(Access { cdn, headers, backup_dir, media_dir }))
    }
}

/// A week of media credentials; `None` when the account has no backup.
async fn fetch_credentials(
    service: &presage::libsignal_service::push_service::PushService,
    root: &BackupKey,
    aci: Aci,
    params: &GenericServerPublicParams,
    now: u64,
) -> anyhow::Result<Option<Vec<(u64, BackupAuthCredential)>>> {
    let (start, end, path) = credentials_path(now);
    let response = service.request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?.send().await?;
    // Only the phone can set the backup id; without it there's no backup
    // (Desktop's `#doFetch` on a linked device).
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body = response.error_for_status().context("getting backup credentials")?.bytes().await?;
    let context = BackupAuthCredentialRequestContext::new(root, aci);
    receive_credentials(&body, &context, params, start, end).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng as _;

    fn root() -> BackupKey {
        BackupKey([7; 32])
    }

    fn hkdf(key: &[u8], info: &[&[u8]], out: &mut [u8]) {
        let info: Vec<u8> = info.concat();
        hkdf::Hkdf::<sha2::Sha256>::new(None, key).expand(&info, out).unwrap();
    }

    #[test]
    fn media_names_and_ids_follow_desktop_and_libsignal() {
        // Desktop's `getMediaName`: hex of the plaintext hash, then the key.
        let name = media_name(&[0xab; 32], &[0x01; 64]);
        assert_eq!(name, format!("{}{}", "ab".repeat(32), "01".repeat(64)));
        assert_eq!(thumbnail_media_name("x"), "x_thumbnail");
        // libsignal's `derive_media_id`: HKDF-SHA256 of the root key with
        // its label and the name, 15 bytes.
        let id = MediaId::new(&root(), &name);
        let mut expected = [0; 15];
        hkdf(&[7; 32], &[b"20241007_SIGNAL_BACKUP_MEDIA_ID:", name.as_bytes()], &mut expected);
        assert_eq!(id.0, expected);
        // Base64url without padding: 15 bytes are 20 characters.
        assert_eq!(id.string().len(), 20);
        assert_eq!(MediaId([0xfb; 15]).string(), "-_v7-_v7-_v7-_v7-_v7");
    }

    #[test]
    fn layer_keys_put_aes_first() {
        let id = MediaId([3; 15]);
        let mut material = [0; 64];
        hkdf(&[7; 32], &[b"20241007_SIGNAL_BACKUP_ENCRYPT_MEDIA:", &id.0], &mut material);
        let key = outer_key(&root(), &id);
        // Desktop: MAC key first, AES second; the cipher takes AES first.
        assert_eq!(&key[..32], &material[32..]);
        assert_eq!(&key[32..], &material[..32]);
        let mut material = [0; 64];
        hkdf(&[7; 32], &[b"20241030_SIGNAL_BACKUP_ENCRYPT_THUMBNAIL:", &id.0], &mut material);
        let key = thumbnail_inner_key(&root(), &id);
        assert_eq!(&key[..32], &material[32..]);
        assert_eq!(&key[32..], &material[..32]);
    }

    /// Encrypts like Signal: the attachment (padded), then the backup layer.
    fn encrypt(key: [u8; 64], plain: &[u8]) -> Vec<u8> {
        let mut data = plain.to_vec();
        presage::libsignal_service::attachment_cipher::encrypt_in_place([9; 16], key, &mut data);
        data
    }

    fn remote_for(plain: &[u8], key: [u8; 64]) -> RemoteAttachment {
        use sha2::Digest as _;
        RemoteAttachment {
            cdn: 0,
            cdn_key: None,
            key: hex::encode(key),
            digest: None,
            plaintext_hash: Some(hex::encode(sha2::Sha256::digest(plain))),
            media_tier_cdn: Some(3),
        }
    }

    #[test]
    fn decrypts_both_layers_and_checks_the_hash() {
        let plain = b"a photo, more or less".to_vec();
        let key = [5; 64];
        let remote = remote_for(&plain, key);
        let mut padded = plain.clone();
        padded.resize(64, 0);
        let inner = encrypt(key, &padded);
        let (hash, _) = media_parts(&remote).unwrap();
        let id = MediaId::new(&root(), &media_name(&hash, &key));
        let outer = encrypt(outer_key(&root(), &id), &inner);
        assert_eq!(decrypt_media(&root(), &remote, plain.len() as u64, outer.clone()).unwrap(), plain);
        // A different root key can't open it.
        assert!(decrypt_media(&BackupKey([8; 32]), &remote, plain.len() as u64, outer.clone()).is_err());
        // Nor does it pass with the wrong hash.
        let wrong = RemoteAttachment { plaintext_hash: Some("00".repeat(32)), ..remote };
        assert!(decrypt_media(&root(), &wrong, plain.len() as u64, outer).is_err());
    }

    #[test]
    fn decrypts_a_thumbnail() {
        let key = [5; 64];
        let remote = remote_for(b"the full-size file", key);
        let (hash, _) = media_parts(&remote).unwrap();
        let id = MediaId::new(&root(), &thumbnail_media_name(&media_name(&hash, &key)));
        let webp = b"RIFF....WEBP".to_vec();
        let inner = encrypt(thumbnail_inner_key(&root(), &id), &webp);
        let outer = encrypt(outer_key(&root(), &id), &inner);
        assert_eq!(decrypt_thumbnail(&root(), &remote, outer).unwrap(), webp);
    }

    #[test]
    fn only_a_key_and_plaintext_hash_name_a_file() {
        let remote = remote_for(b"x", [5; 64]);
        assert!(media_parts(&remote).is_some());
        assert!(media_parts(&RemoteAttachment { plaintext_hash: None, ..remote.clone() }).is_none());
        assert!(media_parts(&RemoteAttachment { key: "00".repeat(32), ..remote }).is_none());
        assert!(can_have_thumbnail("image/jpeg") && can_have_thumbnail("video/mp4"));
        assert!(!can_have_thumbnail("application/pdf"));
    }

    #[test]
    fn ciphertext_sizes_match_desktop() {
        // `getAttachmentCiphertextSize` with `MediaTier.BACKUP`, worked out
        // by hand: 1000 pads to 1020 (then 1020), the inner file is
        // 16 + 1024 + 32 = 1072 bytes, which pads to 1125, so the outer one
        // is 16 + 1136 + 32.
        assert_eq!(crate::long_text::ciphertext_size(1000), 1072);
        assert_eq!(ciphertext_size(1000), 1184);
        assert_eq!(download_limit(1000, false), 1184 + 59);
        assert!(download_limit(0, true) > 16 * 1024);
    }

    #[test]
    fn requests_are_formed_as_desktop_forms_them() {
        // 2026-10-15 12:00 UTC: today and six days on.
        let (start, end, path) = credentials_path(1_792_022_400 + 12 * 3600);
        assert_eq!((start, end), (1_792_022_400, 1_792_022_400 + 6 * 86_400));
        assert_eq!(path, "/v1/archives/auth?redemptionStartSeconds=1792022400&redemptionEndSeconds=1792540800");
        assert_eq!(read_credentials_path(3), "/v1/archives/auth/read?cdn=3");
        let id = MediaId([0xfb; 15]);
        assert_eq!(media_path("abc_-", "def", &id).unwrap(), "backups/abc_-/def/-_v7-_v7-_v7-_v7-_v7");
        assert!(media_path("../x", "def", &id).is_err());
        assert!(media_path("", "def", &id).is_err());
        let headers = parse_read_credentials(br#"{"headers":{"Authorization":"Bearer x","b":"y"}}"#).unwrap();
        assert_eq!(headers, [("Authorization".to_owned(), "Bearer x".to_owned()), ("b".to_owned(), "y".to_owned())]);
        let info = br#"{"cdn":3,"backupDir":"bd","mediaDir":"md","backupName":"n","usedSpace":null}"#;
        assert_eq!(parse_backup_info(info).unwrap(), ("bd".to_owned(), "md".to_owned()));
        use crate::remote_config::RemoteConfig;
        assert_eq!(fallback_cdn(&RemoteConfig::from_pairs([(FALLBACK_CDN_CONFIG, "3"), ("other", "x")])), Some(3));
        assert_eq!(fallback_cdn(&RemoteConfig::from_pairs([])), None);
    }

    #[test]
    fn server_params_parse() {
        // Both are version 1 ("standard"), which needs libsignal v0.100 or
        // later to read.
        for servers in [SignalServers::Production, SignalServers::Staging] {
            assert!(matches!(server_params(servers).unwrap(), GenericServerPublicParams::V1(_)));
        }
    }

    // libsignal's vectors (`rust/net/chat/src/api/backups.rs`, `testutil`):
    // a media credential, its server's public params and a signing key, and
    // what presenting it gives with `fixed_seed_test_rng` (ChaCha20 seeded
    // with 0).
    const TEST_SIGNING_KEY: &str = "KMhdmPEusAwoT3C2LzIbmGX6z+3HMbhgbrXmUwRfGF0=";
    const TEST_SIGNING_KEY_PUB: &str = "BWp7eOx6q6IlijMPozln1bY34JoLFZhGu3PLDnn7hO9t";
    const TEST_SERVER_KEYS: &str = "AIRCHmMrkZXZ9ZuwKJkA0GeMOaDSdVsU26AghADhY3l5XBYwf0UCtm2tvvYsbnPgh9uIUyERm0Wg3v7pFtg+OEfsM6fwjdBFqAgfeqs1pT9nwp2Wp6oGdAfCTrGcqraXJoyAiwAh3vogu7ltucNKh25zKiOkIeIEJNrjbx2eEwkFnqLYuk/noxaOi2Zl7R5d7+vn0Me0d2AZhu0Uuk1vpTIuYf+X4UJXV/N5TYYxwOe/OQHu4zZmdaPjtPN1EHFJC5ALV+8BY9dN5ddS7iTL1uq1ksURAA9hAZzC9/aTr7J7";
    const TEST_CREDENTIAL: &str = "AACkl2kAAAAAyQAAAAAAAAACAAAAAAAAAMUH8mZNP0qDpXFbK2e3dKL04Zw1UhyJ5ab+RlRLhAYELu5/fvwOhxzvxcnNGpqppkGOWc7SSN0kEU0MMIslejR+FDPRx0BWeRTeMmr2ngFVaHUjmazUmgCAPkr0BuLjShTidN9UW8r2M6FjodEtF/8=";
    const EXPECTED_PRESENTATION: &str = "AMkAAAAAAAAAAgAAAAAAAAAApJdpAAAAAIoiVNK2DtZIRFCtQxRiSokkSiQEKrUm86QgMg+qyZZjLuJipcWuggZt6au2i4MOhslTP4qafDZUYWZnKdX7zV4MKW1+FqHVi9kns3+gGaHRCrUEqKcTBzZj/C79ZRJObwIAAAAAAAAA7vpvGr5uokinX1GRCgDr5au1ajuE2naAsAUXPXXpxTyKZo+S3m3OdyDUusIM3sIyUFwM1OeMtmHLgDcuGAqKdYAAAAAAAAAAcqkJSxGNgTB4ERB7Qcg8tp+IZnEhGxCzuvY3KqrjgwA1LniEMcZCO9kjcSL2Q5JS5yZYrv7Kkn0p3hY4vIrKBlgb0zycYLKRrUj+ndkHKJtWV/2xC42jehDUc1P2ufIEJfu4ScD+sUt9fgAV7uDsKI/ktXnhUPT7/ZxtCCp88gEU4nTfVFvK9jOhY6HRLRf/";
    const EXPECTED_SIGNATURE: &str = "TUmhLTMN7LLUOphZiAF8WZekmWzYDWlDiqNm3LirWwcSotw+yUd+MOizCpwVD+Wp9dLHjqU00xUwm+KnxtiKiA==";

    #[test]
    fn presentations_match_libsignals_vectors() {
        let credential: BackupAuthCredential = zkgroup::deserialize(&STANDARD.decode(TEST_CREDENTIAL).unwrap()).unwrap();
        let params = GenericServerPublicParams::try_from(STANDARD.decode(TEST_SERVER_KEYS).unwrap().as_slice()).unwrap();
        let signing_key = PrivateKey::deserialize(&STANDARD.decode(TEST_SIGNING_KEY).unwrap()).unwrap();
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(0);
        let (presentation, signature) = present(&credential, &params, &signing_key, &mut rng).unwrap();
        assert_eq!(STANDARD.encode(&presentation), EXPECTED_PRESENTATION);
        assert_eq!(STANDARD.encode(&signature), EXPECTED_SIGNATURE);
        let headers = auth_headers(&presentation, &signature);
        assert_eq!(headers[0], ("x-signal-zk-auth", EXPECTED_PRESENTATION.to_owned()));
        assert_eq!(headers[1], ("x-signal-zk-auth-signature", EXPECTED_SIGNATURE.to_owned()));
        assert_eq!(credential.backup_level(), BackupLevel::Paid);
        let body = set_public_key_body(&signing_key).unwrap();
        assert_eq!(body, serde_json::json!({ "backupIdPublicKey": TEST_SIGNING_KEY_PUB }));
    }

    #[test]
    fn credentials_are_received_and_checked() {
        use zkgroup::generic_server_params::{
            GenericServerSecretParamsLegacy, GenericServerSecretParamsStandard,
        };
        // Signal's servers use version 1 ("standard") params now; libsignal's
        // vectors are version 0.
        check_credentials(GenericServerSecretParamsStandard::generate([1; 32]).into());
        check_credentials(GenericServerSecretParamsLegacy::generate([2; 32]).into());
    }

    fn check_credentials(server: zkgroup::generic_server_params::GenericServerSecretParams) {
        use zkgroup::backups::BackupCredentialType;
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(1);
        let params = server.get_public_params();
        let aci = Aci::from_uuid_bytes([0x11; 16]);
        let context = BackupAuthCredentialRequestContext::new(&root(), aci);
        let (start, end, _) = credentials_path(1_792_022_400);
        let issue = |day: u64, level: BackupLevel| {
            let response = context.get_request().issue(
                zkgroup::Timestamp::from_epoch_seconds(day),
                level,
                BackupCredentialType::Media,
                &server,
                [day as u8; 32],
            );
            serde_json::json!({ "credential": STANDARD.encode(zkgroup::serialize(&response)), "redemptionTime": day })
        };
        let media: Vec<_> = (0..7).map(|d| issue(start + d * DAY_SECONDS, BackupLevel::Paid)).collect();
        let answer = serde_json::json!({ "credentials": { "messages": [], "media": media } });
        let credentials = receive_credentials(answer.to_string().as_bytes(), &context, &params, start, end).unwrap();
        assert_eq!(credentials.len(), 7);
        assert_eq!(credentials[0].0, start);
        assert!(credentials.iter().all(|(_, c)| c.backup_level() == BackupLevel::Paid));
        // Presenting one works against the same server.
        let (presentation, _) =
            present(&credentials[0].1, &params, &root().derive_ec_key(&aci), &mut rng).unwrap();
        let presentation: zkgroup::backups::BackupAuthCredentialPresentation = zkgroup::deserialize(&presentation).unwrap();
        presentation.verify(zkgroup::Timestamp::from_epoch_seconds(start + 60), &server).unwrap();

        // Desktop refuses a day outside the range, and a day twice.
        let late = serde_json::json!({ "credentials": { "messages": [], "media": [issue(end + DAY_SECONDS, BackupLevel::Paid)] } });
        assert!(receive_credentials(late.to_string().as_bytes(), &context, &params, start, end).is_err());
        let twice = serde_json::json!({ "credentials": { "messages": [], "media": [issue(start, BackupLevel::Paid), issue(start, BackupLevel::Paid)] } });
        assert!(receive_credentials(twice.to_string().as_bytes(), &context, &params, start, end).is_err());
        // One issued for someone else's backup doesn't verify.
        let other = BackupAuthCredentialRequestContext::new(&BackupKey::<1>([9; 32]), aci);
        let answer = serde_json::json!({ "credentials": { "messages": [], "media": [issue(start, BackupLevel::Free)] } });
        assert!(receive_credentials(answer.to_string().as_bytes(), &other, &params, start, end).is_err());
        let free = receive_credentials(answer.to_string().as_bytes(), &context, &params, start, end).unwrap();
        assert_eq!(free[0].1.backup_level(), BackupLevel::Free);
    }
}
