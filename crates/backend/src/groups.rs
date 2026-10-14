//! Changing a group on Signal's group server: its timer, name and
//! description, and leaving it. libsignal-service can fetch and decrypt
//! groups and encrypt each change, but has no call to apply one, so this
//! authenticates and sends `PATCH /v1/groups/` itself (as Desktop's
//! `modifyGroupV2`).
//!
//! Group requests use a zero-knowledge credential for the day: fetched with
//! the account's own credentials, then "presented" for this group, so the
//! server learns neither who is asking nor which group's members they are.

use anyhow::{Context as _, anyhow, bail};
use presage::libsignal_service::{
    configuration::{Endpoint, ServiceConfiguration},
    groups_v2::{AccessControl, AccessRequired, Group, GroupMemberCandidate, GroupOperations, decrypt_group},
    protocol::Aci,
    prelude::ProtobufMessage as _,
    proto::{self, GroupChangeResponse},
    push_service::{HttpAuth, HttpAuthOverride, PushService},
    zkgroup::{
        self, ServerPublicParams, Timestamp,
        auth::{AuthCredentialWithPni, AuthCredentialWithPniResponse},
        groups::{GroupMasterKey, GroupSecretParams},
        profiles::{ExpiringProfileKeyCredential, ExpiringProfileKeyCredentialResponse, ProfileKey},
    },
};
use rand::Rng as _;

use crate::actor::SignalManager;

const DAY_SECONDS: u64 = 24 * 60 * 60;

/// Today's start (UTC), in seconds: the redemption time of today's credential.
pub(crate) fn today(now_seconds: u64) -> u64 {
    now_seconds - now_seconds % DAY_SECONDS
}

/// The base64 credential for `day` in `{"credentials": [{"credential",
/// "redemptionTime"}]}`.
fn credential_for(json: &serde_json::Value, day: u64) -> Option<&str> {
    json["credentials"]
        .as_array()?
        .iter()
        .find(|c| c["redemptionTime"].as_u64() == Some(day))?["credential"]
        .as_str()
}

/// An authenticated connection to one group.
pub(crate) struct GroupSession {
    service: PushService,
    params: ServerPublicParams,
    auth: HttpAuth,
    master_key: [u8; 32],
    pub operations: GroupOperations,
}

impl GroupSession {
    pub(crate) async fn new(manager: &SignalManager, master_key: [u8; 32]) -> anyhow::Result<Self> {
        use base64::Engine as _;
        let service = crate::storage::push_service(manager)?;
        let data = manager.registration_data();
        let params = ServiceConfiguration::from(data.signal_servers).zkgroup_server_public_params;
        let secret = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let today = today(now);

        let path = format!(
            "/v1/certificate/auth/group?redemptionStartSeconds={today}&redemptionEndSeconds={}&pniAsServiceId=true",
            today + 7 * DAY_SECONDS
        );
        let response = service
            .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
            .send()
            .await?
            .error_for_status()?;
        let credentials: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
        let credential = credential_for(&credentials, today).context("no group credential for today")?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(credential)?;
        let response = AuthCredentialWithPniResponse::new(&bytes).map_err(|e| anyhow!("{e:?}"))?;
        let credential = response
            .receive(&params, data.service_ids.aci(), data.service_ids.pni(), Timestamp::from_epoch_seconds(today))
            .map_err(|e| anyhow!("{e:?}"))?;
        // As libsignal-service does: through bytes, to the presentable type.
        let credential = AuthCredentialWithPni::new(&zkgroup::serialize(&credential)).map_err(|e| anyhow!("{e:?}"))?;
        let randomness: [u8; 32] = rand::rng().random();
        let presentation = credential.present(&params, &secret, randomness);
        let auth = HttpAuth {
            username: hex::encode(bincode::serialize(&secret.get_public_params())?),
            password: hex::encode(zkgroup::serialize(&presentation)),
        };
        Ok(Self { service, params, auth, master_key, operations: GroupOperations::new(secret) })
    }

    fn request(&self, method: reqwest::Method) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(self
            .service
            .request(method, Endpoint::storage("/v1/groups/"), HttpAuthOverride::Identified(self.auth.clone()))?
            .header("Content-Type", "application/x-protobuf"))
    }

    /// A member's credential for adding them to a group: proof of their
    /// profile key that the group server checks without learning it.
    async fn profile_key_credential(&self, aci: Aci, key: ProfileKey) -> anyhow::Result<ExpiringProfileKeyCredential> {
        use base64::Engine as _;
        let randomness: [u8; 32] = rand::rng().random();
        let context = self.params.create_profile_key_credential_request_context(randomness, aci, key);
        let path = format!(
            "/v1/profile/{}/{}/{}?credentialType=expiringProfileKey",
            uuid_of(aci),
            key.get_profile_key_version(aci).as_ref(),
            hex::encode(zkgroup::serialize(&context.get_request())),
        );
        let response = self
            .service
            .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
            .send()
            .await?
            .error_for_status()?;
        let json: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
        let credential = json["credential"].as_str().context("no credential in the profile")?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(credential)?;
        let response: ExpiringProfileKeyCredentialResponse =
            zkgroup::deserialize(&bytes).map_err(|e| anyhow!("{e:?}"))?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        self.params
            .receive_expiring_profile_key_credential(&context, &response, Timestamp::from_epoch_seconds(now))
            .map_err(|e| anyhow!("{e:?}"))
    }

    /// Creates this (new) group with us as its admin. Members whose profile
    /// key we have join at once; the rest are invited, as in Desktop.
    pub(crate) async fn create(
        &self,
        title: &str,
        me: (Aci, ProfileKey),
        members: &[(Aci, Option<ProfileKey>)],
    ) -> anyhow::Result<()> {
        let mine = self.profile_key_credential(me.0, me.1).await.context("getting our own credential")?;
        let mut candidates = Vec::new();
        for (aci, key) in members {
            let credential = match key {
                Some(key) => self.profile_key_credential(*aci, *key).await.ok(),
                None => None,
            };
            candidates.push(GroupMemberCandidate { service_id: (*aci).into(), credential });
        }
        // Desktop's defaults: any member may edit details and add people; no link.
        let access = AccessControl {
            attributes: AccessRequired::Member,
            members: AccessRequired::Member,
            add_from_invite_link: AccessRequired::Unsatisfiable,
            member_label: AccessRequired::Member,
        };
        let group = self
            .operations
            .encrypt_group_with_credentials(
                title,
                None,
                None,
                Some(&access),
                &mine,
                &candidates,
                &self.params,
                String::new(),
                &mut rand::rng(),
            )
            .map_err(|e| anyhow!("{e:?}"))?;
        let response = self.request(reqwest::Method::PUT)?.body(group.encode_to_vec()).send().await?;
        if !response.status().is_success() {
            bail!("the group server answered {}", response.status());
        }
        Ok(())
    }

    /// The group as the server has it now.
    pub(crate) async fn fetch(&self) -> anyhow::Result<Group> {
        let response = self.request(reqwest::Method::GET)?.send().await?.error_for_status()?;
        let group = proto::Group::decode(response.bytes().await?)?;
        Ok(decrypt_group(&self.master_key, group)?)
    }

    /// Applies `actions` (for the next revision) and returns the
    /// server-signed change, to send to the group's members.
    pub(crate) async fn modify(&self, actions: proto::group_change::Actions) -> anyhow::Result<Vec<u8>> {
        let response = self.request(reqwest::Method::PATCH)?.body(actions.encode_to_vec()).send().await?;
        match response.status().as_u16() {
            200 => {}
            403 => bail!("you don't have permission to change this group"),
            409 => bail!("the group changed in the meantime; try again"),
            status => bail!("the group server answered {status}"),
        }
        let response = GroupChangeResponse::decode(response.bytes().await?)?;
        Ok(response.group_change.context("no signed change in the answer")?.encode_to_vec())
    }
}

/// Downloads and decrypts a group's photo; `path` is the group's `avatar`
/// (empty when it has none). The CDN needs no credential.
pub(crate) async fn download_avatar(manager: &SignalManager, master_key: [u8; 32], path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let service = crate::storage::push_service(manager)?;
    let response = service
        .request(reqwest::Method::GET, Endpoint::cdn(0, path), HttpAuthOverride::Unidentified)?
        .send()
        .await?
        .error_for_status()?;
    let encrypted = response.bytes().await?;
    let operations = GroupOperations::new(GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key)));
    Ok(operations.decrypt_avatar(&encrypted))
}

fn uuid_of(aci: Aci) -> presage::libsignal_service::prelude::Uuid {
    aci.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_per_utc_day() {
        assert_eq!(today(1_700_000_000), 1_699_920_000);
        assert_eq!(today(1_699_920_000), 1_699_920_000);
    }

    #[test]
    fn picks_todays_credential() {
        let json = serde_json::json!({"credentials": [
            {"credential": "b2xk", "redemptionTime": 86_400},
            {"credential": "dG9kYXk=", "redemptionTime": 172_800}
        ]});
        assert_eq!(credential_for(&json, 172_800), Some("dG9kYXk="));
        assert_eq!(credential_for(&json, 0), None);
    }
}
