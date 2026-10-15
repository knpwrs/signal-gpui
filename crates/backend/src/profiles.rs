//! Profile photos, fetched fresh. presage keeps the first profile and photo
//! it downloads for good, so a contact's new photo never showed; this asks
//! the server for the profile's current avatar path (a new one for every
//! upload) and downloads that.

use anyhow::{Context as _, anyhow};
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::prelude::ProfileKey;
use presage::libsignal_service::profile_cipher::ProfileCipher;
use presage::libsignal_service::protocol::Aci;
use presage::libsignal_service::push_service::HttpAuthOverride;

use crate::actor::SignalManager;

/// Their profile as the server has it now.
async fn profile(manager: &SignalManager, aci: Aci, key: ProfileKey) -> anyhow::Result<serde_json::Value> {
    let service = crate::storage::push_service(manager)?;
    let version = key.get_profile_key_version(aci);
    let uuid: presage::libsignal_service::prelude::Uuid = aci.into();
    let path = format!("/v1/profile/{uuid}/{}", version.as_ref());
    let response = service
        .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
        .send()
        .await?
        .error_for_status()?;
    Ok(serde_json::from_slice(&response.bytes().await?)?)
}

/// The CDN path of their current profile photo, `None` when they have none.
pub(crate) async fn avatar_path(manager: &SignalManager, aci: Aci, key: ProfileKey) -> anyhow::Result<Option<String>> {
    let profile = profile(manager, aci, key).await?;
    Ok(profile["avatar"].as_str().filter(|p| !p.is_empty()).map(str::to_owned))
}

/// Whether their profile has donor badges (what Desktop checks of ours for
/// a donation megaphone).
pub(crate) async fn has_badges(manager: &SignalManager, aci: Aci, key: ProfileKey) -> anyhow::Result<bool> {
    let profile = profile(manager, aci, key).await?;
    Ok(profile["badges"].as_array().is_some_and(|b| !b.is_empty()))
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

/// The saved photo's file name (without extension): one per person and
/// avatar path, so a new photo gets a new file.
pub(crate) fn file_stem(service_id: &str, avatar_path: &str) -> String {
    use sha2::Digest as _;
    format!("d-{service_id}-{}", &hex::encode(sha2::Sha256::digest(avatar_path.as_bytes()))[..16])
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_new_photo_gets_a_new_file() {
        let a = super::file_stem("abc", "profiles/one");
        assert!(a.starts_with("d-abc-"));
        assert_eq!(a, super::file_stem("abc", "profiles/one"));
        assert_ne!(a, super::file_stem("abc", "profiles/two"));
    }
}
