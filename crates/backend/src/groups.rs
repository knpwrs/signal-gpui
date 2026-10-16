//! Changing a group on Signal's group server: its timer, name,
//! description and photo, its members, admins, permissions and link,
//! invitations and requests to join, and joining by a link. libsignal-service can fetch and decrypt
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
    pub params: ServerPublicParams,
    auth: HttpAuth,
    master_key: [u8; 32],
    pub secret: GroupSecretParams,
    pub operations: GroupOperations,
}

/// Why a group link's group can't be shown (Desktop's `joinViaLink` errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinInfoError {
    /// An admin removed us: the server says why (`X-Signal-Forbidden-Reason`).
    Banned,
    /// The link was reset or turned off (403).
    Revoked,
    /// The group was ended (423).
    Ended,
    /// Anything else (the network, the server).
    Failed,
}

impl std::fmt::Display for JoinInfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text().1)
    }
}

impl std::error::Error for JoinInfoError {}

impl JoinInfoError {
    /// Desktop's title and description for each.
    pub(crate) fn text(self) -> (&'static str, &'static str) {
        match self {
            JoinInfoError::Banned => (
                "Can’t Join Group",
                "You can't join this group via the group link because an admin removed you. Contact an admin to be added back to the group.",
            ),
            JoinInfoError::Revoked => ("Can’t Join Group", "This group link is no longer valid."),
            JoinInfoError::Ended => ("Error", "Unable to join this group."),
            JoinInfoError::Failed => ("Link Error", "Couldn't join group. Try again later."),
        }
    }
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
        Ok(Self { service, params, auth, master_key, secret, operations: GroupOperations::new(secret) })
    }

    fn request(&self, method: reqwest::Method) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(self
            .service
            .request(method, Endpoint::storage("/v1/groups/"), HttpAuthOverride::Identified(self.auth.clone()))?
            .header("Content-Type", "application/x-protobuf"))
    }

    /// A member's credential for adding them to a group: proof of their
    /// profile key that the group server checks without learning it.
    pub(crate) async fn profile_key_credential(&self, aci: Aci, key: ProfileKey) -> anyhow::Result<ExpiringProfileKeyCredential> {
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
        avatar: Option<&[u8]>,
        timer: u32,
    ) -> anyhow::Result<()> {
        // Desktop's `createGroupV2`: the photo goes up before the group.
        let avatar = match avatar {
            Some(image) => self.upload_avatar(image).await.context("uploading the group photo")?,
            None => String::new(),
        };
        let timer = (timer > 0).then_some(presage::libsignal_service::groups_v2::Timer { duration: timer });
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
                timer.as_ref(),
                Some(&access),
                &mine,
                &candidates,
                &self.params,
                avatar,
                &mut rand::rng(),
            )
            .map_err(|e| anyhow!("{e:?}"))?;
        let response = self.request(reqwest::Method::PUT)?.body(group.encode_to_vec()).send().await?;
        if !response.status().is_success() {
            bail!("the group server answered {}", response.status());
        }
        Ok(())
    }

    /// Actions adding people to this group, as Desktop's
    /// `buildAddMembersChange` (`ts/groups.preload.ts`): those whose profile
    /// key we have (and whose credential we get) join at once, the rest are
    /// invited; anyone banned is unbanned.
    pub(crate) async fn add_members_actions(
        &self,
        current: &Group,
        me: Aci,
        people: &[(Aci, Option<ProfileKey>)],
        actions: &mut proto::group_change::Actions,
    ) -> anyhow::Result<()> {
        use presage::libsignal_service::groups_v2::Role;
        for (aci, key) in people {
            let credential = match key {
                Some(key) => self.profile_key_credential(*aci, *key).await.ok(),
                None => None,
            };
            match credential {
                Some(credential) => actions.add_members.push(self.operations.build_add_member_action_with_credential(
                    &credential,
                    Role::Default,
                    &self.params,
                )),
                None => actions.add_members_pending_profile_key.push(
                    self.operations
                        .build_add_pending_member_action((*aci).into(), me, Role::Default)
                        .map_err(|e| anyhow!("{e:?}"))?,
                ),
            }
            let service_id: presage::libsignal_service::protocol::ServiceId = (*aci).into();
            if current.members_banned.iter().any(|b| b.user_id == service_id) {
                // The encrypted id, as an invitation's removal names it.
                let deleted_user_id = self
                    .operations
                    .build_remove_pending_member_action(service_id)
                    .map_err(|e| anyhow!("{e:?}"))?
                    .deleted_user_id;
                actions.delete_members_banned.push(proto::group_change::actions::DeleteMemberBannedAction { deleted_user_id });
            }
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
        self.modify_with_link(actions, None).await
    }

    /// [`Self::modify`] as someone joining or asking to join by the link
    /// (`password`: the link's), as Desktop's `modifyGroup` sends it
    /// (`?inviteLinkPassword=`).
    pub(crate) async fn modify_with_link(&self, actions: proto::group_change::Actions, password: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
        use base64::Engine as _;
        // URL-safe base64 needs no escaping.
        let path = match password {
            Some(password) => {
                format!("/v1/groups/?inviteLinkPassword={}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(password))
            }
            None => "/v1/groups/".to_owned(),
        };
        let response = self
            .service
            .request(reqwest::Method::PATCH, Endpoint::storage(path), HttpAuthOverride::Identified(self.auth.clone()))?
            .header("Content-Type", "application/x-protobuf")
            .body(actions.encode_to_vec())
            .send()
            .await?;
        match response.status().as_u16() {
            200 => {}
            400 => {
                let body = response.text().await.unwrap_or_default();
                // Desktop's `joinGroupV2ViaLink`: the server's word for a
                // request that's already there.
                if body.contains("cannot ask to join via invite link if already asked to join") {
                    return Err(AlreadyRequested.into());
                }
                bail!("the group server refused the change");
            }
            403 => bail!("you don't have permission to change this group"),
            409 => bail!("the group changed in the meantime; try again"),
            status => bail!("the group server answered {status}"),
        }
        let response = GroupChangeResponse::decode(response.bytes().await?)?;
        Ok(response.group_change.context("no signed change in the answer")?.encode_to_vec())
    }

    /// What a group link shows before joining (Desktop's
    /// `getPreJoinGroupInfo`, `GET /v1/groups/join/{password}`).
    pub(crate) async fn join_info(&self, password: &[u8]) -> Result<proto::GroupJoinInfo, JoinInfoError> {
        use base64::Engine as _;
        let path = format!("/v1/groups/join/{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(password));
        let response = self
            .service
            .request(reqwest::Method::GET, Endpoint::storage(path), HttpAuthOverride::Identified(self.auth.clone()))
            .map_err(|_| JoinInfoError::Failed)?
            .header("Content-Type", "application/x-protobuf")
            .send()
            .await
            .map_err(|_| JoinInfoError::Failed)?;
        match response.status().as_u16() {
            200 => {}
            403 if response.headers().contains_key("x-signal-forbidden-reason") => return Err(JoinInfoError::Banned),
            403 => return Err(JoinInfoError::Revoked),
            423 => return Err(JoinInfoError::Ended),
            _ => return Err(JoinInfoError::Failed),
        }
        let bytes = response.bytes().await.map_err(|_| JoinInfoError::Failed)?;
        proto::GroupJoinInfo::decode(bytes).map_err(|_| JoinInfoError::Failed)
    }

    /// Uploads a group photo (Desktop's `uploadGroupAvatar`: a form from
    /// `GET /v1/groups/avatar/form`, then a POST to CDN 0) and returns the
    /// key the group's `avatar` names it by.
    pub(crate) async fn upload_avatar(&self, image: &[u8]) -> anyhow::Result<String> {
        let encrypted = crate::group_admin::encrypt_avatar(&self.secret, image);
        let response = self
            .service
            .request(
                reqwest::Method::GET,
                Endpoint::storage("/v1/groups/avatar/form"),
                HttpAuthOverride::Identified(self.auth.clone()),
            )?
            .send()
            .await?
            .error_for_status()?;
        let form = proto::AvatarUploadAttributes::decode(response.bytes().await?)?;
        // Desktop's `verifyAttributes`.
        let fields = [&form.key, &form.credential, &form.acl, &form.algorithm, &form.date, &form.policy, &form.signature];
        if fields.iter().any(|f| f.is_empty()) {
            bail!("the upload form is missing a field");
        }
        // Desktop's `makePutParams`, in its order.
        let body = reqwest::multipart::Form::new()
            .text("key", form.key.clone())
            .text("x-amz-credential", form.credential)
            .text("acl", form.acl)
            .text("x-amz-algorithm", form.algorithm)
            .text("x-amz-date", form.date)
            .text("policy", form.policy)
            .text("x-amz-signature", form.signature)
            .text("Content-Type", "application/octet-stream")
            .part("file", reqwest::multipart::Part::bytes(encrypted).mime_str("application/octet-stream")?);
        self.service
            .request(reqwest::Method::POST, Endpoint::cdn(0, "/"), HttpAuthOverride::Unidentified)?
            .multipart(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(form.key)
    }
}

/// The server already has our request to join.
#[derive(Debug)]
pub(crate) struct AlreadyRequested;

impl std::fmt::Display for AlreadyRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("already asked to join")
    }
}

impl std::error::Error for AlreadyRequested {}

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

/// Signal's `GroupExternalCredential` (Groups.proto): a group call's
/// membership proof.
#[derive(Clone, PartialEq, prost::Message)]
struct GroupExternalCredential {
    #[prost(string, tag = "1")]
    token: String,
}

impl GroupSession {
    /// A group call's membership proof (`GET /v1/groups/token`, as
    /// Desktop's `getGroupExternalCredential`): the token, as bytes.
    pub(crate) async fn call_proof(&self) -> anyhow::Result<Vec<u8>> {
        let response = self
            .service
            .request(reqwest::Method::GET, Endpoint::storage("/v1/groups/token"), HttpAuthOverride::Identified(self.auth.clone()))?
            .header("Content-Type", "application/x-protobuf")
            .send()
            .await?
            .error_for_status()?;
        let credential = GroupExternalCredential::decode(response.bytes().await?)?;
        Ok(credential.token.into_bytes())
    }
}

/// A group's identifier (what RingRTC calls the group id).
pub(crate) fn group_identifier(master_key: [u8; 32]) -> Vec<u8> {
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key)).get_group_identifier().to_vec()
}

/// A group call's member list for RingRTC: each ACI with its ciphertext
/// under the group's key (as the group server lists members).
pub(crate) fn call_members(master_key: [u8; 32], members: &[Aci]) -> Vec<signal_calls::GroupMember> {
    let secret = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key));
    members
        .iter()
        .map(|aci| signal_calls::GroupMember {
            user_id: uuid_of(*aci).into_bytes().to_vec(),
            member_id: zkgroup::serialize(&secret.encrypt_service_id((*aci).into())),
        })
        .collect()
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
