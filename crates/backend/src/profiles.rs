//! Profiles: reading anyone's (name, about, badges, photo) fresh from the
//! server, and writing ours, as Signal Desktop does.
//!
//! presage keeps the first profile and photo it downloads for good, so a
//! contact's new photo never showed; this asks the server for the profile
//! as it is now, whose avatar path is new for every upload.
//!
//! Writing follows Desktop's `writeProfile` and `encryptProfileData`
//! (`ts/services/writeProfile.preload.ts`, `ts/util/encryptProfileData.preload.ts`):
//! `PUT /v1/profile` with the versioned profile, each field encrypted with
//! our profile key and padded to Desktop's lengths, then the photo
//! (encrypted, unpadded) posted to the CDN with the form the server
//! answered. The profile key is the account's (the primary's): a linked
//! device never makes a new one.

use aes_gcm::{Aes256Gcm, KeyInit as _, aead::Aead as _};
use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::prelude::ProfileKey;
use presage::libsignal_service::profile_cipher::ProfileCipher;
use presage::libsignal_service::profile_name::ProfileName;
use presage::libsignal_service::protocol::Aci;
use presage::libsignal_service::push_service::{AttachmentV2UploadAttributes, HttpAuthOverride};
use rand::CryptoRng;
use signal_store::{Badge, BadgeCategory, BadgeImage, FetchedProfile, ProfileBadge};

use crate::actor::SignalManager;

/// Where badge images are (Desktop's `updatesUrl` and `parseImageFilename`).
const BADGE_IMAGES: &str = "https://updates2.signal.org/static/badges/";

/// Desktop's field limits (`ProfileEditor.dom.tsx`): each name is at most
/// 26 characters and 128 bytes, the about text 140 and 512.
pub const NAME_GRAPHEMES: usize = 26;
pub const NAME_BYTES: usize = 128;
pub const ABOUT_GRAPHEMES: usize = 140;
pub const ABOUT_BYTES: usize = 512;

/// Someone's profile as the server has it now.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RemoteProfile {
    /// The CDN path of their profile photo.
    pub avatar: Option<String>,
    pub profile: FetchedProfile,
    /// The badges their profile names, described.
    pub badges: Vec<Badge>,
    /// Our own profile's payment address, kept as the server has it
    /// (encrypted) so a write keeps it, as Desktop does.
    pub payment_address: Option<String>,
}

/// Their profile, fetched with their profile key.
pub(crate) async fn fetch(manager: &SignalManager, aci: Aci, key: ProfileKey) -> anyhow::Result<RemoteProfile> {
    let service = crate::storage::push_service(manager)?;
    let version = key.get_profile_key_version(aci);
    let uuid: presage::libsignal_service::prelude::Uuid = aci.into();
    let path = format!("/v1/profile/{uuid}/{}", version.as_ref());
    let response = service
        .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
        .send()
        .await?
        .error_for_status()?;
    let json: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    Ok(decode(&json, key))
}

/// A profile response decrypted with their key. Fields that don't decrypt
/// are left empty (Desktop logs and skips them too).
pub(crate) fn decode(json: &serde_json::Value, key: ProfileKey) -> RemoteProfile {
    let field = |name: &str| -> Option<Vec<u8>> {
        let text = json[name].as_str().filter(|s| !s.is_empty())?;
        let data = BASE64_STANDARD.decode(text).ok()?;
        decrypt(&data, key)
    };
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).ok().map(|s| s.trim_end_matches('\0').to_owned());
    let (given_name, family_name) = field("name")
        .map(|name| {
            let name = trim_padding(&name);
            // Desktop's `decryptProfileName`: given name up to the first
            // NUL, the family name after it.
            let mut parts = name.splitn(2, |b| *b == 0);
            let given = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
            let family = parts.next().map(|f| String::from_utf8_lossy(f).into_owned()).unwrap_or_default();
            (given, family)
        })
        .unwrap_or_default();
    let (owned, badges) = parse_badges(&json["badges"]);
    RemoteProfile {
        avatar: json["avatar"].as_str().filter(|p| !p.is_empty()).map(str::to_owned),
        profile: FetchedProfile {
            given_name,
            family_name,
            about: field("about").and_then(text).unwrap_or_default(),
            about_emoji: field("aboutEmoji").and_then(text).unwrap_or_default(),
            badges: owned,
        },
        badges,
        payment_address: json["paymentAddress"].as_str().filter(|p| !p.is_empty()).map(str::to_owned),
    }
}

/// The plaintext of an encrypted profile field (`iv || ciphertext`, AES-GCM
/// with the profile key), or `None` (Desktop's `decryptProfile`).
fn decrypt(data: &[u8], key: ProfileKey) -> Option<Vec<u8>> {
    if data.len() < 12 + 16 + 1 {
        return None;
    }
    let (nonce, ciphertext) = data.split_first_chunk::<12>()?;
    Aes256Gcm::new((&key.get_bytes()).into()).decrypt(nonce.into(), ciphertext).ok()
}

/// `data` without its trailing zero padding.
fn trim_padding(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    &data[..end]
}

/// `iv || AES-GCM(data)` with a fresh IV (Desktop's `encryptProfile`).
fn encrypt(data: &[u8], key: ProfileKey, rng: &mut impl CryptoRng) -> anyhow::Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    rng.fill_bytes(&mut nonce);
    let ciphertext =
        Aes256Gcm::new((&key.get_bytes()).into()).encrypt((&nonce).into(), data).map_err(|_| anyhow!("can't encrypt"))?;
    Ok([nonce.as_slice(), &ciphertext].concat())
}

/// The badges in a profile (Desktop's `parseBadgesFromServer`): which they
/// have, and each one's description. At most 1000, as Desktop reads.
fn parse_badges(value: &serde_json::Value) -> (Vec<ProfileBadge>, Vec<Badge>) {
    let mut owned = Vec::new();
    let mut badges = Vec::new();
    for item in value.as_array().map(Vec::as_slice).unwrap_or_default().iter().take(1000) {
        let Some(badge) = parse_badge(item) else { continue };
        // Desktop keeps `expiresAt` and `isVisible` only when both are there
        // (they are on our own profile).
        let (expires_at, visible) = match (item["expiration"].as_f64(), item["visible"].as_bool()) {
            (Some(expiration), Some(visible)) => (Some((expiration * 1000.) as i64), Some(visible)),
            _ => (None, None),
        };
        owned.push(ProfileBadge { id: badge.id.clone(), expires_at, visible });
        badges.push(badge);
    }
    (owned, badges)
}

fn parse_badge(item: &serde_json::Value) -> Option<Badge> {
    let string = |name: &str| item[name].as_str().map(str::to_owned);
    let svgs = item["svgs"].as_array().filter(|s| s.len() == 3)?;
    let url = |file: &str| -> Option<String> {
        // A file name, not a path: it has to stay under /static/badges/
        // (Desktop's `isBadgeImageFileUrlValid`).
        (!file.is_empty() && !file.contains(['/', '\\', '?', '#']) && file != "..").then(|| format!("{BADGE_IMAGES}{file}"))
    };
    let mut images = Vec::new();
    for svg in svgs {
        let theme = |name: &str| svg[name].as_str().and_then(url);
        let image = BadgeImage { light: theme("light"), dark: theme("dark"), transparent: theme("transparent") };
        if image != BadgeImage::default() {
            images.push(image);
        }
    }
    images.push(BadgeImage { transparent: Some(url(item["svg"].as_str()?)?), ..Default::default() });
    if images.len() != 4 {
        return None;
    }
    Some(Badge {
        id: string("id")?,
        category: BadgeCategory::parse(&string("category")?),
        name: string("name")?,
        description: string("description")?,
        images,
    })
}

/// The badge each donation level stands for, from the subscription
/// configuration (Desktop's `parseBoostBadgeListFromServer`,
/// `ts/badges/parseBadgesFromServer.std.ts`): a level without a badge it
/// can read is left out.
pub(crate) fn level_badges(config: &serde_json::Value) -> std::collections::HashMap<u64, Badge> {
    let Some(levels) = config["levels"].as_object() else { return Default::default() };
    levels
        .iter()
        .filter_map(|(level, item)| Some((level.parse().ok()?, parse_badge(&item["badge"])?)))
        .collect()
}

/// Fetches the subscription configuration (`GET /v1/subscription/configuration`,
/// unauthenticated, as Desktop's `getSubscriptionConfiguration`) and reads
/// its donation levels' badges.
pub(crate) async fn fetch_level_badges(manager: &SignalManager) -> anyhow::Result<std::collections::HashMap<u64, Badge>> {
    let service = crate::storage::push_service(manager)?;
    let response = service
        .request(reqwest::Method::GET, Endpoint::service("/v1/subscription/configuration"), HttpAuthOverride::Unidentified)?
        .send()
        .await?
        .error_for_status()?;
    let config: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    Ok(level_badges(&config))
}

/// Whether their profile has donor badges (what Desktop checks of ours for
/// a donation megaphone).
pub(crate) async fn has_badges(manager: &SignalManager, aci: Aci, key: ProfileKey) -> anyhow::Result<bool> {
    Ok(!fetch(manager, aci, key).await?.profile.badges.is_empty())
}

/// Downloads and decrypts the photo at `path` with their profile key.
pub(crate) async fn download_avatar(manager: &SignalManager, path: &str, key: ProfileKey) -> anyhow::Result<Vec<u8>> {
    let service = crate::storage::push_service(manager)?;
    let response = service
        .request(reqwest::Method::GET, Endpoint::cdn(0, path), HttpAuthOverride::Unidentified)?
        .send()
        .await?
        .error_for_status()?;
    let encrypted = response.bytes().await?;
    let photo = ProfileCipher::new(key).decrypt_avatar(&encrypted).map_err(|e| anyhow!("{e:?}"))?;
    Some(photo).filter(|p| !p.is_empty()).context("empty photo")
}

/// Downloads a badge image from updates2.signal.org.
pub(crate) async fn download_badge_image(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    if !url.starts_with(BADGE_IMAGES) {
        bail!("badge image isn't on updates2.signal.org");
    }
    let response = client.get(url).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}

/// The saved photo's file name (without extension): one per person and
/// avatar path, so a new photo gets a new file.
pub(crate) fn file_stem(service_id: &str, avatar_path: &str) -> String {
    use sha2::Digest as _;
    format!("d-{service_id}-{}", &hex::encode(sha2::Sha256::digest(avatar_path.as_bytes()))[..16])
}

/// Largest side of a profile photo (Desktop's `processImageFile`:
/// `maxWidth: 512, maxHeight: 512`).
pub const AVATAR_SIZE: u32 = 512;

/// A picked image as Desktop uploads it (`processImageFile`): cropped to
/// the middle square (`cover`, `crop`), scaled down to at most 512 pixels
/// a side, and saved as a JPEG (`canvasToBytes`, the browser's default
/// quality, 0.92). Transparent parts come out black, as on Desktop's
/// canvas.
pub fn avatar_image(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory(bytes).context("not an image")?;
    let side = image.width().min(image.height());
    if side < 2 {
        bail!("the image is too small");
    }
    let (x, y) = ((image.width() - side) / 2, (image.height() - side) / 2);
    let mut square = image.crop_imm(x, y, side, side);
    if side > AVATAR_SIZE {
        square = square.resize_exact(AVATAR_SIZE, AVATAR_SIZE, image::imageops::FilterType::Triangle);
    }
    let mut rgba = square.to_rgba8();
    for pixel in rgba.pixels_mut() {
        // Over black, as a JPEG canvas flattens it.
        let alpha = u16::from(pixel.0[3]);
        for channel in &mut pixel.0[..3] {
            *channel = (u16::from(*channel) * alpha / 255) as u8;
        }
    }
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&rgb)?;
    Ok(out)
}

/// What to do with our profile photo in a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarChange {
    /// Keep the one we have (Desktop's `sameAvatar`).
    Keep,
    /// No photo.
    Remove,
    /// A new photo: a JPEG or PNG, already cropped.
    New(std::sync::Arc<Vec<u8>>),
}

/// Our profile as the editor saves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEdit {
    pub given_name: String,
    pub family_name: String,
    pub about: String,
    pub about_emoji: String,
    pub avatar: AvatarChange,
}

/// The body of `PUT /v1/profile` (Desktop's `ProfileRequestDataType`),
/// and the encrypted photo to upload, if any.
pub(crate) struct ProfileRequest {
    pub body: serde_json::Value,
    pub avatar: Option<Vec<u8>>,
}

/// What a write keeps of our profile as it is: Desktop fetches it first so
/// a write keeps our badges' visibility and our payment address.
pub(crate) struct Kept<'a> {
    /// We have a photo now (`Keep` without one writes none).
    pub has_avatar: bool,
    /// Our visible badges.
    pub badge_ids: &'a [String],
    pub payment_address: Option<&'a str>,
    /// The account shares our number with everybody.
    pub sharing_number: bool,
}

/// Desktop's `encryptProfileData`.
pub(crate) fn profile_request(
    edit: &ProfileEdit,
    aci: Aci,
    key: ProfileKey,
    kept: &Kept<'_>,
    rng: &mut impl CryptoRng,
) -> anyhow::Result<ProfileRequest> {
    let Kept { has_avatar, badge_ids, payment_address, sharing_number } = *kept;
    let given = edit.given_name.trim();
    if given.is_empty() {
        bail!("a profile needs a first name");
    }
    let family = edit.family_name.trim();
    let cipher = ProfileCipher::new(key);
    let name = ProfileName { given_name: given, family_name: Some(family).filter(|f| !f.is_empty()) };
    let name = cipher.encrypt_name(&name, rng).map_err(|e| anyhow!("name: {e}"))?;
    let about = edit.about.trim();
    let about = (!about.is_empty())
        .then(|| cipher.encrypt_about(about.to_owned(), rng))
        .transpose()
        .map_err(|e| anyhow!("about: {e}"))?;
    let emoji = (!edit.about_emoji.is_empty() && !edit.about.trim().is_empty())
        .then(|| cipher.encrypt_emoji(edit.about_emoji.clone(), rng))
        .transpose()
        .map_err(|e| anyhow!("about emoji: {e}"))?;
    let sharing = encrypt(&[u8::from(sharing_number)], key, rng)?;
    let (avatar, same_avatar, upload) = match &edit.avatar {
        AvatarChange::Keep => (has_avatar, has_avatar, None),
        AvatarChange::Remove => (false, false, None),
        AvatarChange::New(bytes) => (true, false, Some(encrypt(bytes, key, rng)?)),
    };
    let version = key.get_profile_key_version(aci);
    let commitment = bincode::serialize(&key.get_commitment(aci))?;
    let b64 = |bytes: &[u8]| BASE64_STANDARD.encode(bytes);
    let body = serde_json::json!({
        "version": version.as_ref(),
        "name": b64(&name),
        "about": about.as_deref().map(b64),
        "aboutEmoji": emoji.as_deref().map(b64),
        "badgeIds": badge_ids,
        "paymentAddress": payment_address,
        "avatar": avatar,
        "sameAvatar": same_avatar,
        "commitment": b64(&commitment),
        "phoneNumberSharing": b64(&sharing),
    });
    Ok(ProfileRequest { body, avatar: upload })
}

/// Writes our profile; returns the photo's new CDN path when one was
/// uploaded (Desktop's `uploadAvatar` returns the form's `key`).
pub(crate) async fn write(manager: &SignalManager, request: ProfileRequest) -> anyhow::Result<Option<String>> {
    let service = crate::storage::push_service(manager)?;
    let response = service
        .request(reqwest::Method::PUT, Endpoint::service("/v1/profile"), HttpAuthOverride::NoOverride)?
        .json(&request.body)
        .send()
        .await?
        .error_for_status()?;
    let body = response.bytes().await?;
    let Some(avatar) = request.avatar else { return Ok(None) };
    // With a new photo the answer is its upload form.
    let form: AttachmentV2UploadAttributes = serde_json::from_slice(&body).context("no upload form for the photo")?;
    let key = serde_json::from_slice::<serde_json::Value>(&body)?["key"].as_str().context("upload form has no key")?.to_owned();
    let mut service = service.clone();
    service.upload_to_cdn0("/", form, "file".into(), std::io::Cursor::new(avatar)).await?;
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng as _;

    use super::*;

    const ACI: &str = "6b1a2b3c-0000-4000-8000-000000000001";

    fn key() -> ProfileKey {
        ProfileKey::create([9; 32])
    }

    fn aci() -> Aci {
        Aci::parse_from_service_id_string(ACI).unwrap()
    }

    fn edit() -> ProfileEdit {
        ProfileEdit {
            given_name: " Sam ".into(),
            family_name: "Rivera".into(),
            about: "Out hiking".into(),
            about_emoji: "🥾".into(),
            avatar: AvatarChange::Keep,
        }
    }

    fn request(edit: &ProfileEdit, has_avatar: bool) -> ProfileRequest {
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(1);
        let kept = Kept { has_avatar, badge_ids: &["R_LOW".into()], payment_address: Some("cGF5"), sharing_number: true };
        profile_request(edit, aci(), key(), &kept, &mut rng).unwrap()
    }

    fn field(body: &serde_json::Value, name: &str) -> Vec<u8> {
        BASE64_STANDARD.decode(body[name].as_str().unwrap()).unwrap()
    }

    #[test]
    fn a_written_profile_reads_back_with_libsignal() {
        let request = request(&edit(), true);
        let body = &request.body;
        // libsignal-service's own decryption reads what we wrote.
        let cipher = ProfileCipher::new(key());
        let name = cipher.decrypt_name(field(body, "name")).unwrap().unwrap();
        assert_eq!((name.given_name.as_str(), name.family_name.as_deref()), ("Sam", Some("Rivera")));
        assert_eq!(cipher.decrypt_about(field(body, "about")).unwrap(), "Out hiking");
        assert_eq!(cipher.decrypt_emoji(field(body, "aboutEmoji")).unwrap(), "🥾");
        // Padded to Desktop's lengths: 12 IV bytes, the padded text, 16 tag bytes.
        assert_eq!(field(body, "name").len(), 12 + 53 + 16);
        assert_eq!(field(body, "about").len(), 12 + 128 + 16);
        assert_eq!(field(body, "aboutEmoji").len(), 12 + 32 + 16);
        // Phone number sharing is one unpadded byte.
        assert_eq!(decrypt(&field(body, "phoneNumberSharing"), key()), Some(vec![1]));
        // And our own decoding of a profile reads it too.
        let read = decode(body, key());
        assert_eq!(
            (read.profile.given_name.as_str(), read.profile.family_name.as_str(), read.profile.about.as_str()),
            ("Sam", "Rivera", "Out hiking")
        );
        assert_eq!(read.profile.about_emoji, "🥾");
    }

    #[test]
    fn the_request_names_the_key_version_and_commitment() {
        let request = request(&edit(), true);
        let body = &request.body;
        let version = key().get_profile_key_version(aci());
        assert_eq!(body["version"].as_str(), Some(version.as_ref()));
        assert_eq!(body["version"].as_str().unwrap().len(), 64, "hex");
        let commitment = bincode::serialize(&key().get_commitment(aci())).unwrap();
        assert_eq!(field(body, "commitment"), commitment);
        assert_eq!(body["badgeIds"], serde_json::json!(["R_LOW"]));
        assert_eq!(body["paymentAddress"], "cGF5");
        // Keeping the photo we have.
        assert_eq!((body["avatar"].as_bool(), body["sameAvatar"].as_bool()), (Some(true), Some(true)));
        assert!(request.avatar.is_none());
    }

    #[test]
    fn photos_are_kept_removed_or_replaced() {
        let no_photo = request(&edit(), false);
        assert_eq!((no_photo.body["avatar"].as_bool(), no_photo.body["sameAvatar"].as_bool()), (Some(false), Some(false)));
        let removed = request(&ProfileEdit { avatar: AvatarChange::Remove, ..edit() }, true);
        assert_eq!((removed.body["avatar"].as_bool(), removed.body["sameAvatar"].as_bool()), (Some(false), Some(false)));
        let photo = std::sync::Arc::new(b"\x89PNG photo".to_vec());
        let new = request(&ProfileEdit { avatar: AvatarChange::New(photo.clone()), ..edit() }, true);
        assert_eq!((new.body["avatar"].as_bool(), new.body["sameAvatar"].as_bool()), (Some(true), Some(false)));
        // The upload is the photo, unpadded, as libsignal decrypts avatars.
        let upload = new.avatar.unwrap();
        assert_eq!(upload.len(), 12 + photo.len() + 16);
        assert_eq!(ProfileCipher::new(key()).decrypt_avatar(&upload).unwrap(), *photo);
    }

    #[test]
    fn empty_fields_are_left_out_as_desktop_does() {
        let request = request(&ProfileEdit { family_name: " ".into(), about: String::new(), ..edit() }, false);
        assert!(request.body["about"].is_null());
        // No about text, no emoji (Desktop clears the emoji with the text).
        assert!(request.body["aboutEmoji"].is_null());
        let name = ProfileCipher::new(key()).decrypt_name(field(&request.body, "name")).unwrap().unwrap();
        assert_eq!((name.given_name.as_str(), name.family_name), ("Sam", None));
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(1);
        let nameless = ProfileEdit { given_name: "  ".into(), ..edit() };
        let kept = Kept { has_avatar: false, badge_ids: &[], payment_address: None, sharing_number: false };
        assert!(profile_request(&nameless, aci(), key(), &kept, &mut rng).is_err());
    }

    #[test]
    fn picked_images_are_cropped_square_and_scaled() {
        let encode = |width: u32, height: u32| {
            let image = image::RgbaImage::from_fn(width, height, |x, _| image::Rgba([(x % 255) as u8, 80, 160, 255]));
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        let wide = avatar_image(&encode(900, 600)).unwrap();
        assert!(wide.starts_with(&[0xff, 0xd8]), "a JPEG");
        let decoded = image::load_from_memory(&wide).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (512, 512));
        // Small ones keep their size, squared.
        let small = image::load_from_memory(&avatar_image(&encode(40, 64)).unwrap()).unwrap();
        assert_eq!((small.width(), small.height()), (40, 40));
        assert!(avatar_image(b"not an image").is_err());
    }

    #[test]
    fn badges_parse_as_desktop_reads_them() {
        let json = serde_json::json!({
            "badges": [
                {
                    "id": "R_LOW", "category": "donor", "name": "Signal Sustainer",
                    "description": "{short_name} supports Signal", "svg": "sustainer.svg",
                    "svgs": [{"light": "s1l.svg", "dark": "s1d.svg"}, {"light": "s2l.svg"}, {"dark": "s3d.svg"}],
                    "expiration": 1700000000, "visible": false
                },
                { "id": "BAD", "category": "testing", "name": "x", "description": "y", "svg": "z.svg", "svgs": [] },
                { "id": "ESCAPE", "category": "other", "name": "x", "description": "y", "svg": "../x.svg",
                  "svgs": [{"light": "a"}, {"light": "b"}, {"light": "c"}] }
            ],
            "avatar": "profiles/abc"
        });
        let profile = decode(&json, key());
        assert_eq!(profile.avatar.as_deref(), Some("profiles/abc"));
        assert_eq!(profile.badges.len(), 1, "the other two are malformed");
        let badge = &profile.badges[0];
        assert_eq!(badge.category, BadgeCategory::Donor);
        assert_eq!(badge.images.len(), 4);
        assert_eq!(badge.images[0].light.as_deref(), Some("https://updates2.signal.org/static/badges/s1l.svg"));
        assert_eq!(badge.images[3].transparent.as_deref(), Some("https://updates2.signal.org/static/badges/sustainer.svg"));
        assert_eq!(
            profile.profile.badges,
            [ProfileBadge { id: "R_LOW".into(), expires_at: Some(1_700_000_000_000), visible: Some(false) }]
        );
        // Undecryptable or missing fields stay empty.
        assert_eq!(profile.profile.given_name, "");
    }

    #[test]
    fn donation_levels_name_their_badges() {
        let config = serde_json::json!({
            "levels": {
                "100": { "name": "Gift", "badge": {
                    "id": "BOOST", "category": "donor", "name": "Signal Boost", "description": "{short_name} donated",
                    "svg": "boost.svg", "svgs": [{"light": "b1"}, {"light": "b2"}, {"light": "b3"}]
                } },
                "200": { "name": "Broken", "badge": { "id": "X" } },
                "many": { "badge": {} }
            }
        });
        let badges = level_badges(&config);
        assert_eq!(badges.keys().collect::<Vec<_>>(), [&100]);
        assert_eq!(badges[&100].id, "BOOST");
        assert!(level_badges(&serde_json::json!({})).is_empty());
    }
}
