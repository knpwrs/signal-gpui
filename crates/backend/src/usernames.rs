//! Our username and its link, as Signal Desktop sets them
//! (`ts/services/username.preload.ts`, `ts/util/Username.dom.ts`,
//! `ts/types/Username.std.ts`): a nickname is reserved with candidate
//! discriminators (or the one typed), confirmed with a zero-knowledge proof
//! and a fresh encrypted link, and deleted; the link can be reset and
//! colored. The account record keeps the username and the link (entropy,
//! server id, color) for our other devices.

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::push_service::HttpAuthOverride;
use rand::{CryptoRng, Rng};
use usernames::{NicknameLimits, Username, UsernameError};

use crate::actor::SignalManager;

/// Settings key: our username link, `{entropy, server_id, color}` as JSON
/// (Desktop's `usernameLink` and `usernameLinkColor` items).
pub(crate) const SETTING_USERNAME_LINK: &str = "account.username_link";

/// Most digits a discriminator may have (Desktop's `DISCRIMINATOR_MAX_LENGTH`).
pub const DISCRIMINATOR_MAX_LENGTH: usize = 9;

/// Why a username can't be reserved (Desktop's `UsernameReservationError`),
/// each with Desktop's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsernameProblem {
    NotEnoughCharacters { min: usize },
    TooManyCharacters { max: usize },
    CheckStartingCharacter,
    CheckCharacters,
    UsernameNotAvailable,
    NotEnoughDiscriminator,
    AllZeroDiscriminator,
    LeadingZeroDiscriminator,
    TooManyAttempts,
    /// Couldn't save (network, server): Desktop's general error dialog.
    General,
    /// The reservation lapsed or someone took it (`ConflictOrGone`).
    ConflictOrGone,
}

impl UsernameProblem {
    /// Desktop's words for it (`UsernameEditor`'s `errorString`, and the
    /// dialogs for the last two).
    pub fn message(self, username: &str) -> String {
        match self {
            Self::NotEnoughCharacters { min } => {
                format!("Usernames must have at least {min} character{}.", if min == 1 { "" } else { "s" })
            }
            Self::TooManyCharacters { max } => {
                format!("Usernames must have at most {max} character{}.", if max == 1 { "" } else { "s" })
            }
            Self::CheckStartingCharacter => "Usernames cannot begin with a number.".into(),
            Self::CheckCharacters => "Usernames may only contain a-z, 0-9 and _".into(),
            Self::UsernameNotAvailable => "This username is not available".into(),
            Self::NotEnoughDiscriminator => "Invalid username, enter a minimum of 2 digits.".into(),
            Self::AllZeroDiscriminator => "This number can’t be 00. Enter a digit between 1-9".into(),
            Self::LeadingZeroDiscriminator => "Numbers with more than 2 digits can’t start with 0".into(),
            Self::TooManyAttempts => "Too many attempts made, please try again later".into(),
            Self::General => "Your username couldn’t be saved. Check your connection and try again.".into(),
            Self::ConflictOrGone => format!(
                "{username} is no longer available. A new set of digits will be paired with your username, please try saving it again."
            ),
        }
    }

    /// Shown under the field (the others are dialogs in Desktop).
    pub fn inline(self) -> bool {
        !matches!(self, Self::General | Self::ConflictOrGone)
    }

    /// Desktop's mapping from libsignal's errors (`reserveUsername`).
    fn from_error(error: &UsernameError, limits: (usize, usize)) -> Self {
        match error {
            UsernameError::NicknameCannotBeEmpty | UsernameError::NicknameTooShort => {
                Self::NotEnoughCharacters { min: limits.0 }
            }
            UsernameError::NicknameTooLong => Self::TooManyCharacters { max: limits.1 },
            UsernameError::NicknameCannotStartWithDigit => Self::CheckStartingCharacter,
            UsernameError::BadNicknameCharacter | UsernameError::MissingSeparator | UsernameError::BadDiscriminatorCharacter => {
                Self::CheckCharacters
            }
            UsernameError::DiscriminatorCannotBeZero => Self::AllZeroDiscriminator,
            UsernameError::DiscriminatorCannotHaveLeadingZeros => Self::LeadingZeroDiscriminator,
            UsernameError::DiscriminatorCannotBeEmpty
            | UsernameError::DiscriminatorCannotBeSingleDigit
            | UsernameError::DiscriminatorTooLarge => Self::NotEnoughDiscriminator,
        }
    }
}

/// The part before the dot (Desktop's `getNickname`).
pub fn nickname_of(username: &str) -> &str {
    username.split('.').next().unwrap_or_default()
}

/// The digits after the last dot (Desktop's `getDiscriminator`).
pub fn discriminator_of(username: &str) -> Option<&str> {
    let (_, digits) = username.rsplit_once('.')?;
    digits.chars().all(|c| c.is_ascii_digit()).then_some(digits)
}

/// The nickname limits from remote config, made safe for libsignal (which
/// asserts `min < max <= 48`).
pub fn limits() -> (usize, usize) {
    let config = crate::remote_config();
    clamp_limits(config.username_nickname_min(), config.username_nickname_max())
}

fn clamp_limits(min: usize, max: usize) -> (usize, usize) {
    let max = max.clamp(2, 48);
    (min.clamp(1, max - 1), max)
}

/// A username reserved for us, not yet ours (Desktop's
/// `UsernameReservationType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub username: String,
    pub previous: Option<String>,
    pub hash: [u8; 32],
}

impl Reservation {
    /// Only the nickname's case changed (Desktop's `isCaseChange`): the
    /// server's hash is the same, so only the link is rewritten.
    pub fn is_case_change(&self) -> bool {
        self.previous.as_ref().is_some_and(|p| p.to_lowercase() == self.username.to_lowercase())
    }
}

/// What to ask the server to reserve: the usernames to try, or (a case
/// change of ours) the reservation needing no request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReserveRequest {
    Candidates(Vec<String>),
    CaseChange(Reservation),
}

/// Desktop's `reserveUsername` up to the request: a case change keeps our
/// digits; digits typed are tried alone; otherwise libsignal's candidates.
pub(crate) fn reserve_request(
    nickname: &str,
    custom_discriminator: Option<&str>,
    previous: Option<&str>,
    limits: (usize, usize),
    rng: &mut impl Rng,
) -> Result<ReserveRequest, UsernameProblem> {
    let custom = custom_discriminator.filter(|d| !d.is_empty());
    if let (Some(previous), None) = (previous, custom) {
        let previous_nickname = nickname_of(previous);
        if previous_nickname.to_lowercase() == nickname.to_lowercase()
            && let Some(digits) = discriminator_of(previous)
        {
            let username = format!("{nickname}.{digits}");
            let parsed = Username::new(&username).map_err(|e| UsernameProblem::from_error(&e, limits))?;
            return Ok(ReserveRequest::CaseChange(Reservation {
                username,
                previous: Some(previous.to_owned()),
                hash: parsed.hash(),
            }));
        }
    }
    let nickname_limits = NicknameLimits::new(limits.0, limits.1);
    let candidates = match custom {
        Some(digits) => {
            // libsignal asserts the nickname is ASCII before measuring it.
            if !nickname.is_ascii() {
                return Err(UsernameProblem::CheckCharacters);
            }
            vec![Username::from_parts(nickname, digits, nickname_limits).map_err(|e| UsernameProblem::from_error(&e, limits))?.to_string()]
        }
        None => Username::candidates_from(rng, nickname, nickname_limits).map_err(|e| UsernameProblem::from_error(&e, limits))?,
    };
    Ok(ReserveRequest::Candidates(candidates))
}

/// The body of `PUT /v1/accounts/username_hash/reserve`.
pub(crate) fn reserve_body(candidates: &[String]) -> anyhow::Result<(serde_json::Value, Vec<[u8; 32]>)> {
    let hashes = candidates.iter().map(|c| Username::new(c).map(|u| u.hash())).collect::<Result<Vec<_>, _>>()?;
    let encoded: Vec<String> = hashes.iter().map(|h| BASE64_URL_SAFE_NO_PAD.encode(h)).collect();
    Ok((serde_json::json!({ "usernameHashes": encoded }), hashes))
}

fn decode_b64url(text: &str) -> anyhow::Result<Vec<u8>> {
    Ok(BASE64_URL_SAFE_NO_PAD.decode(text.trim_end_matches('='))?)
}

/// What the server answered, as a status and its body.
async fn send(request: reqwest::RequestBuilder) -> anyhow::Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
    let response = request.send().await?;
    let status = response.status();
    let headers = response.headers().clone();
    Ok((status, headers, response.bytes().await?.to_vec()))
}

fn rate_limited(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 413 | 429)
}

/// Reserves one of `candidates`; returns which.
pub(crate) async fn reserve(
    manager: &SignalManager,
    candidates: Vec<String>,
    previous: Option<String>,
) -> Result<Reservation, UsernameProblem> {
    let general = |error: anyhow::Error| {
        tracing::warn!(error = %format!("{error:#}"), "couldn't reserve a username");
        UsernameProblem::General
    };
    let (body, hashes) = reserve_body(&candidates).map_err(general)?;
    let service = crate::storage::push_service(manager).map_err(general)?;
    let request = service
        .request(reqwest::Method::PUT, Endpoint::service("/v1/accounts/username_hash/reserve"), HttpAuthOverride::NoOverride)
        .map_err(|e| general(e.into()))?
        .json(&body);
    let (status, _, bytes) = send(request).await.map_err(general)?;
    match status.as_u16() {
        200..=299 => {}
        409 => return Err(UsernameProblem::UsernameNotAvailable),
        // Desktop's `Unprocessable`.
        422 => return Err(UsernameProblem::CheckCharacters),
        _ if rate_limited(status) => return Err(UsernameProblem::TooManyAttempts),
        _ => return Err(general(anyhow!("reserving a username: {status}"))),
    }
    let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| general(e.into()))?;
    let hash = json["usernameHash"].as_str().context("no usernameHash").and_then(decode_b64url).map_err(general)?;
    let index = hashes.iter().position(|h| h.as_slice() == hash).ok_or_else(|| {
        tracing::warn!("reserved a username hash we didn't ask for");
        UsernameProblem::CheckCharacters
    })?;
    Ok(Reservation { username: candidates[index].clone(), previous, hash: hashes[index] })
}

/// Our username link's parts (Desktop's `usernameLink` item plus its
/// color), as the account record's `UsernameLink` keeps them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsernameLink {
    #[serde(with = "hex_bytes")]
    pub entropy: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub server_id: Vec<u8>,
    /// `UsernameLink.Color` (0 unknown, 1 blue, ... 8 purple).
    #[serde(default)]
    pub color: i32,
}

mod hex_bytes {
    pub fn serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text: String = serde::Deserialize::deserialize(d)?;
        hex::decode(text).map_err(serde::de::Error::custom)
    }
}

impl UsernameLink {
    /// `https://signal.me/#eu/<entropy || server id, base64url>`
    /// (libsignal's `generate_username_link`; Desktop's `getUsernameLink`).
    pub fn url(&self) -> Option<String> {
        (self.entropy.len() == 32 && self.server_id.len() == 16).then(|| {
            let payload = [self.entropy.as_slice(), &self.server_id].concat();
            format!("https://signal.me/#eu/{}", BASE64_URL_SAFE_NO_PAD.encode(payload))
        })
    }
}

/// The link's new encrypted username and its entropy: fresh, or (a case
/// change) the old entropy kept.
pub(crate) fn encrypt_for_link(
    username: &str,
    entropy: Option<&[u8; 32]>,
    rng: &mut (impl Rng + CryptoRng),
) -> anyhow::Result<([u8; 32], Vec<u8>)> {
    usernames::create_for_username(rng, username.to_owned(), entropy).map_err(|e| anyhow!("{e}"))
}

/// Makes a reservation ours (Desktop's `confirmUsername`): a case change
/// only rewrites the link (keeping its handle and entropy); anything else
/// is confirmed with a proof and a new link. Returns the new link.
pub(crate) async fn confirm(
    manager: &SignalManager,
    reservation: &Reservation,
    previous_link: Option<&UsernameLink>,
) -> Result<UsernameLink, UsernameProblem> {
    let general = |error: anyhow::Error| {
        tracing::warn!(error = %format!("{error:#}"), "couldn't confirm a username");
        UsernameProblem::General
    };
    let parsed = Username::new(&reservation.username).map_err(|e| general(anyhow!("{e}")))?;
    if parsed.hash() != reservation.hash {
        return Err(general(anyhow!("username hash mismatch")));
    }
    let previous_entropy: Option<[u8; 32]> = previous_link.and_then(|l| l.entropy.as_slice().try_into().ok());
    if reservation.is_case_change()
        && let Some(entropy) = previous_entropy
    {
        let (entropy, encrypted) = encrypt_for_link(&reservation.username, Some(&entropy), &mut rand::rng()).map_err(general)?;
        let server_id = put_link(manager, &encrypted, true).await.map_err(|e| match e {
            LinkError::NoUsername => UsernameProblem::ConflictOrGone,
            LinkError::Other(error) => general(error),
        })?;
        return Ok(UsernameLink { entropy: entropy.to_vec(), server_id, color: previous_link.map_or(0, |l| l.color) });
    }
    let (entropy, encrypted) = encrypt_for_link(&reservation.username, None, &mut rand::rng()).map_err(general)?;
    let randomness: [u8; 32] = rand::rng().random();
    let proof = parsed.proof(&randomness).map_err(|e| general(anyhow!("{e}")))?;
    let body = serde_json::json!({
        "usernameHash": BASE64_URL_SAFE_NO_PAD.encode(reservation.hash),
        "zkProof": BASE64_URL_SAFE_NO_PAD.encode(&proof),
        "encryptedUsername": BASE64_URL_SAFE_NO_PAD.encode(&encrypted),
    });
    let service = crate::storage::push_service(manager).map_err(general)?;
    // Desktop waits out a rate limit and tries again; this does so up to
    // three times, for a minute at most each.
    for _ in 0..3 {
        let request = service
            .request(reqwest::Method::PUT, Endpoint::service("/v1/accounts/username_hash/confirm"), HttpAuthOverride::NoOverride)
            .map_err(|e| general(e.into()))?
            .json(&body);
        let (status, headers, bytes) = send(request).await.map_err(general)?;
        match status.as_u16() {
            200..=299 => {
                let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| general(e.into()))?;
                let handle = json["usernameLinkHandle"]
                    .as_str()
                    .and_then(|h| uuid::Uuid::parse_str(h).ok())
                    .context("no usernameLinkHandle")
                    .map_err(general)?;
                return Ok(UsernameLink { entropy: entropy.to_vec(), server_id: handle.as_bytes().to_vec(), color: previous_link.map_or(0, |l| l.color) });
            }
            409 | 410 => return Err(UsernameProblem::ConflictOrGone),
            _ if rate_limited(status) => {
                let wait = retry_after(&headers).unwrap_or(5);
                if wait > 60 {
                    return Err(UsernameProblem::TooManyAttempts);
                }
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
            _ => return Err(general(anyhow!("confirming a username: {status}"))),
        }
    }
    Err(UsernameProblem::TooManyAttempts)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()
}

pub(crate) enum LinkError {
    /// 409: we have no username to link to.
    NoUsername,
    Other(anyhow::Error),
}

/// `PUT /v1/accounts/username_link`: sets the link's encrypted username;
/// returns its handle (the server id), new unless `keep_handle`.
pub(crate) async fn put_link(manager: &SignalManager, encrypted: &[u8], keep_handle: bool) -> Result<Vec<u8>, LinkError> {
    let body = serde_json::json!({
        "usernameLinkEncryptedValue": BASE64_URL_SAFE_NO_PAD.encode(encrypted),
        "keepLinkHandle": keep_handle,
    });
    let service = crate::storage::push_service(manager).map_err(LinkError::Other)?;
    let request = service
        .request(reqwest::Method::PUT, Endpoint::service("/v1/accounts/username_link"), HttpAuthOverride::NoOverride)
        .map_err(|e| LinkError::Other(e.into()))?
        .json(&body);
    let (status, _, bytes) = send(request).await.map_err(LinkError::Other)?;
    if status.as_u16() == 409 {
        return Err(LinkError::NoUsername);
    }
    if !status.is_success() {
        return Err(LinkError::Other(anyhow!("setting the username link: {status}")));
    }
    let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| LinkError::Other(e.into()))?;
    let handle = json["usernameLinkHandle"]
        .as_str()
        .and_then(|h| uuid::Uuid::parse_str(h).ok())
        .ok_or_else(|| LinkError::Other(anyhow!("no usernameLinkHandle")))?;
    Ok(handle.as_bytes().to_vec())
}

/// `DELETE /v1/accounts/username_hash` (Desktop's `deleteUsername`).
pub(crate) async fn delete(manager: &SignalManager) -> anyhow::Result<()> {
    let service = crate::storage::push_service(manager)?;
    service
        .request(reqwest::Method::DELETE, Endpoint::service("/v1/accounts/username_hash"), HttpAuthOverride::NoOverride)?
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Desktop's `isUsernameValid`, for usernames from elsewhere (the account
/// record): a valid nickname within the limits and two or more digits
/// without leading zeros.
pub fn is_valid(username: &str, limits: (usize, usize)) -> bool {
    let nickname = nickname_of(username);
    let Some(digits) = discriminator_of(username) else { return false };
    let nickname_ok = (limits.0..=limits.1).contains(&nickname.len())
        && nickname.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && nickname.chars().skip(1).all(|c| c.is_ascii_alphanumeric() || c == '_')
        && nickname.len() >= 2;
    let digits_ok = !digits.is_empty() && !digits.starts_with("00") && !(digits.starts_with('0') && digits.len() != 2);
    nickname_ok && digits_ok
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng as _;

    use super::*;

    fn rng() -> rand_chacha::ChaCha20Rng {
        rand_chacha::ChaCha20Rng::seed_from_u64(3)
    }

    #[test]
    fn names_split_as_desktop_splits_them() {
        assert_eq!((nickname_of("trail_mix.42"), discriminator_of("trail_mix.42")), ("trail_mix", Some("42")));
        assert_eq!(discriminator_of("trail_mix"), None);
        assert!(is_valid("trail_mix.42", (3, 32)));
        assert!(is_valid("trail_mix.07", (3, 32)));
        assert!(!is_valid("trail_mix.007", (3, 32)));
        assert!(!is_valid("trail_mix.00", (3, 32)));
        assert!(!is_valid("tm.42", (3, 32)));
        assert!(!is_valid("9lives.42", (3, 32)));
    }

    #[test]
    fn candidates_hash_and_match_the_reservation() {
        let ReserveRequest::Candidates(candidates) = reserve_request("trail_mix", None, None, (3, 32), &mut rng()).unwrap() else {
            panic!("not a case change");
        };
        // libsignal's 4 + 3 + 3 + 2 * 5 candidates over its digit ranges.
        assert_eq!(candidates.len(), 20);
        assert!(candidates.iter().all(|c| c.starts_with("trail_mix.") && is_valid(c, (3, 32))));
        let (body, hashes) = reserve_body(&candidates).unwrap();
        let sent = body["usernameHashes"].as_array().unwrap();
        assert_eq!(sent.len(), 20);
        // URL-safe base64 without padding, of each candidate's hash.
        assert_eq!(decode_b64url(sent[3].as_str().unwrap()).unwrap(), hashes[3]);
        assert_eq!(hashes[3], Username::new(&candidates[3]).unwrap().hash());
        assert!(!sent[0].as_str().unwrap().contains('='));
    }

    #[test]
    fn typed_digits_are_tried_alone() {
        let request = reserve_request("trail_mix", Some("123"), None, (3, 32), &mut rng()).unwrap();
        assert_eq!(request, ReserveRequest::Candidates(vec!["trail_mix.123".into()]));
    }

    #[test]
    fn a_case_change_keeps_our_digits_without_asking() {
        let request = reserve_request("Trail_Mix", None, Some("trail_mix.42"), (3, 32), &mut rng()).unwrap();
        let ReserveRequest::CaseChange(reservation) = request else { panic!("a case change") };
        assert_eq!(reservation.username, "Trail_Mix.42");
        assert!(reservation.is_case_change());
        // The server's hash doesn't change with case.
        assert_eq!(reservation.hash, Username::new("trail_mix.42").unwrap().hash());
        // New digits make it a new username.
        let other = reserve_request("Trail_Mix", Some("43"), Some("trail_mix.42"), (3, 32), &mut rng()).unwrap();
        assert_eq!(other, ReserveRequest::Candidates(vec!["Trail_Mix.43".into()]));
    }

    #[test]
    fn problems_are_desktops() {
        let problem = |nickname: &str, digits: Option<&str>| reserve_request(nickname, digits, None, (3, 32), &mut rng()).unwrap_err();
        assert_eq!(problem("ab", None), UsernameProblem::NotEnoughCharacters { min: 3 });
        assert_eq!(problem(&"a".repeat(33), None), UsernameProblem::TooManyCharacters { max: 32 });
        assert_eq!(problem("9lives", None), UsernameProblem::CheckStartingCharacter);
        assert_eq!(problem("trail-mix", None), UsernameProblem::CheckCharacters);
        assert_eq!(problem("trailmix", Some("7")), UsernameProblem::NotEnoughDiscriminator);
        assert_eq!(problem("trailmix", Some("00")), UsernameProblem::AllZeroDiscriminator);
        assert_eq!(problem("trailmix", Some("012")), UsernameProblem::LeadingZeroDiscriminator);
        assert_eq!(problem("trailmixé", Some("12")), UsernameProblem::CheckCharacters);
        // Remote config sets the limits.
        assert_eq!(
            reserve_request("abcd", None, None, (5, 10), &mut rng()).unwrap_err(),
            UsernameProblem::NotEnoughCharacters { min: 5 }
        );
        assert_eq!(UsernameProblem::NotEnoughCharacters { min: 3 }.message(""), "Usernames must have at least 3 characters.");
        assert!(UsernameProblem::ConflictOrGone.message("trail_mix.42").starts_with("trail_mix.42 is no longer available"));
        assert_eq!(clamp_limits(40, 90), (40, 48));
        assert_eq!(clamp_limits(9, 4), (3, 4));
    }

    #[test]
    fn links_encrypt_and_format_as_libsignal_reads_them() {
        let (entropy, encrypted) = encrypt_for_link("trail_mix.42", None, &mut rng()).unwrap();
        assert_eq!(usernames::decrypt_username(&entropy, &encrypted).unwrap(), "trail_mix.42");
        // A case change keeps the entropy.
        let (kept, _) = encrypt_for_link("Trail_Mix.42", Some(&entropy), &mut rng()).unwrap();
        assert_eq!(kept, entropy);
        let handle = uuid::uuid!("9d0652a3-dcc3-4d11-975f-74d61598733f");
        let link = UsernameLink { entropy: entropy.to_vec(), server_id: handle.as_bytes().to_vec(), color: 4 };
        let url = link.url().unwrap();
        assert!(url.starts_with("https://signal.me/#eu/"));
        let payload = decode_b64url(url.trim_start_matches("https://signal.me/#eu/")).unwrap();
        assert_eq!((&payload[..32], &payload[32..]), (entropy.as_slice(), handle.as_bytes().as_slice()));
        // Kept as JSON in settings.
        let json = serde_json::to_string(&link).unwrap();
        assert_eq!(serde_json::from_str::<UsernameLink>(&json).unwrap(), link);
        assert_eq!(UsernameLink { server_id: vec![1], ..link }.url(), None);
    }

    #[test]
    fn proofs_verify_against_the_hash() {
        let username = Username::new("trail_mix.42").unwrap();
        let proof = username.proof(&[5; 32]).unwrap();
        assert!(Username::verify_proof(&proof, username.hash()).is_ok());
    }
}
