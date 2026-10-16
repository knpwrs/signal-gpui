//! Call links' credentials, as Desktop gets them: a zero-knowledge
//! credential for the day from the chat server (`callLinkAuthCredentials`
//! beside the group credentials, `ts/services/groupCredentialFetcher`),
//! presented for each link (`ts/util/callLinks/zkgroup.preload.ts`), and a
//! one-off credential to create a link (`POST /v1/call-link/create-auth`,
//! `createCallLink` in `ts/services/calling.preload.ts`). The calling
//! server checks them without learning who we are.

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use presage::libsignal_service::{
    configuration::{Endpoint, ServiceConfiguration},
    protocol::Aci,
    push_service::HttpAuthOverride,
    zkgroup::{
        self, Timestamp,
        call_links::{
            CallLinkAuthCredential, CallLinkAuthCredentialResponse, CallLinkSecretParams,
            CreateCallLinkCredentialRequestContext, CreateCallLinkCredentialResponse,
        },
        generic_server_params::GenericServerPublicParams,
    },
};
use rand::Rng as _;

use crate::actor::SignalManager;

/// Signal's production `genericServerPublicParams` as Desktop had them
/// before libsignal 0.101 (`config/production.json` in Desktop 7.60).
/// Desktop 8.34 has newer ones (version byte 1) that only libsignal 0.101
/// and later read, with credentials asked for with `v101=true`; our
/// zkgroup is 0.99, so we use these and ask without it.
const GENERIC_SERVER_PUBLIC_PARAMS: &str = "AByD873dTilmOSG0TjKrvpeaKEsUmIO8Vx9BeMmftwUs9v7ikPwM8P3OHyT0+X3EUMZrSe9VUp26Wai51Q9I8mdk0hX/yo7CeFGJyzoOqn8e/i4Ygbn5HoAyXJx5eXfIbqpc0bIxzju4H/HOQeOpt6h742qii5u/cbwOhFZCsMIbElZTaeU+BWMBQiZHIGHT5IE0qCordQKZ5iPZom0HeFa8Yq0ShuEyAl0WINBiY6xE3H/9WnvzXBbMuuk//eRxXgzO8ieCeK8FwQNxbfXqZm6Ro1cMhCOF3u7xoX83QhpN";

const DAY_SECONDS: u64 = 24 * 60 * 60;

fn generic_params() -> anyhow::Result<GenericServerPublicParams> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(GENERIC_SERVER_PUBLIC_PARAMS)?;
    zkgroup::deserialize(&bytes).map_err(|e| anyhow!("{e:?}"))
}

/// Today's call link credential (serialized), for `day` (unix seconds).
#[derive(Clone)]
pub(crate) struct AuthCredential {
    pub day: u64,
    credential: Vec<u8>,
}

/// The base64 call link credential for `day` in the group credentials'
/// answer (`{"callLinkAuthCredentials": [{"credential", "redemptionTime"}]}`).
fn link_credential_for(json: &serde_json::Value, day: u64) -> Option<&str> {
    json["callLinkAuthCredentials"]
        .as_array()?
        .iter()
        .find(|c| c["redemptionTime"].as_u64() == Some(day))?["credential"]
        .as_str()
}

fn receive_auth_credential(response: &[u8], aci: Aci, day: u64, params: &GenericServerPublicParams) -> anyhow::Result<Vec<u8>> {
    let response: CallLinkAuthCredentialResponse = zkgroup::deserialize(response).map_err(|e| anyhow!("{e:?}"))?;
    let credential = response
        .receive(aci, Timestamp::from_epoch_seconds(day), params)
        .map_err(|_| anyhow!("the call link credential didn't verify"))?;
    Ok(zkgroup::serialize(&credential))
}

/// Fetches today's call link credential, as Desktop's
/// `maybeFetchNewCredentials` does with `zkcCredential=true` (without
/// `v101=true`; see [`GENERIC_SERVER_PUBLIC_PARAMS`]).
pub(crate) async fn fetch_auth_credential(manager: &SignalManager) -> anyhow::Result<AuthCredential> {
    let service = crate::storage::push_service(manager)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let today = crate::groups::today(now);
    let path = format!(
        "/v1/certificate/auth/group?redemptionStartSeconds={today}&redemptionEndSeconds={}&zkcCredential=true",
        today + 7 * DAY_SECONDS
    );
    let response = service
        .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
        .send()
        .await?
        .error_for_status()?;
    let json: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    let credential = link_credential_for(&json, today).context("no call link credential for today")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(credential)?;
    let aci = manager.registration_data().service_ids.aci();
    Ok(AuthCredential { day: today, credential: receive_auth_credential(&bytes, aci, today, &generic_params()?)? })
}

/// Today's credential presented for one link (a
/// `CallLinkAuthCredentialPresentation`), for reading, changing, deleting,
/// peeking and joining it.
pub(crate) fn auth_presentation(credential: &AuthCredential, aci: Aci, root_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    let stored: CallLinkAuthCredential = zkgroup::deserialize(&credential.credential).map_err(|e| anyhow!("{e:?}"))?;
    let secret = CallLinkSecretParams::derive_from_root_key(root_key);
    let randomness: [u8; 32] = rand::rng().random();
    let presentation =
        stored.present(aci, Timestamp::from_epoch_seconds(credential.day), &generic_params()?, &secret, randomness);
    Ok(zkgroup::serialize(&presentation))
}

/// What creating a link needs: the create credential presented for its
/// room, and the link's public params.
pub(crate) struct CreateAuth {
    pub presentation: Vec<u8>,
    pub public_params: Vec<u8>,
}

fn present_create(
    context: CreateCallLinkCredentialRequestContext,
    response: &[u8],
    room_id: &[u8],
    aci: Aci,
    root_key: &[u8],
    params: &GenericServerPublicParams,
) -> anyhow::Result<CreateAuth> {
    let response: CreateCallLinkCredentialResponse = zkgroup::deserialize(response).map_err(|e| anyhow!("{e:?}"))?;
    let credential = context.receive(response, aci, params).map_err(|_| anyhow!("the create credential didn't verify"))?;
    let secret = CallLinkSecretParams::derive_from_root_key(root_key);
    let randomness: [u8; 32] = rand::rng().random();
    let presentation = credential.present(room_id, aci, params, &secret, randomness);
    Ok(CreateAuth {
        presentation: zkgroup::serialize(&presentation),
        public_params: zkgroup::serialize(&secret.get_public_params()),
    })
}

/// A credential to create the link with `root_key` (whose room is
/// `room_id`), from `POST /v1/call-link/create-auth`.
pub(crate) async fn create_auth(manager: &SignalManager, room_id: &[u8], root_key: &[u8]) -> anyhow::Result<CreateAuth> {
    let randomness: [u8; 32] = rand::rng().random();
    let context = CreateCallLinkCredentialRequestContext::new(room_id, randomness);
    let request = base64::engine::general_purpose::STANDARD.encode(zkgroup::serialize(&context.get_request()));
    let service = crate::storage::push_service(manager)?;
    let response = service
        .request(reqwest::Method::POST, Endpoint::service("/v1/call-link/create-auth"), HttpAuthOverride::NoOverride)?
        .json(&serde_json::json!({ "createCallLinkCredentialRequest": request }))
        .send()
        .await?
        .error_for_status()?;
    let json: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    let credential = json["credential"].as_str().context("no credential in the answer")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(credential)?;
    let aci = manager.registration_data().service_ids.aci();
    present_create(context, &bytes, room_id, aci, root_key, &generic_params()?)
}

/// The server's endorsement public key, for joining a link's call
/// (Desktop's `serverPublicParams.getEndorsementPublicKey()`).
pub(crate) fn endorsement_key(manager: &SignalManager) -> Vec<u8> {
    let params = ServiceConfiguration::from(manager.registration_data().signal_servers).zkgroup_server_public_params;
    zkgroup::serialize(&params.get_endorsement_public_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::zkgroup::generic_server_params::GenericServerSecretParams;

    fn aci() -> Aci {
        Aci::from_uuid_bytes([7; 16])
    }

    #[test]
    fn signals_params_parse() {
        assert!(generic_params().is_ok());
    }

    #[test]
    fn picks_todays_link_credential() {
        let json = serde_json::json!({
            "credentials": [{"credential": "Z3JvdXA=", "redemptionTime": 86_400}],
            "callLinkAuthCredentials": [
                {"credential": "b2xk", "redemptionTime": 0},
                {"credential": "dG9kYXk=", "redemptionTime": 86_400}
            ]
        });
        assert_eq!(link_credential_for(&json, 86_400), Some("dG9kYXk="));
        assert_eq!(link_credential_for(&json, 172_800), None);
    }

    /// The whole exchange against a made-up server: what we present is
    /// what the calling server accepts.
    #[test]
    fn credentials_present_as_the_server_checks_them() {
        let server = GenericServerSecretParams::generate([1; 32]);
        let public = server.get_public_params();
        let day = 1_700_006_400 - 1_700_006_400 % DAY_SECONDS;
        let root_key = [9u8; 16];
        let link_public = CallLinkSecretParams::derive_from_root_key(&root_key).get_public_params();

        let issued = CallLinkAuthCredentialResponse::issue_credential(aci(), Timestamp::from_epoch_seconds(day), &server, [2; 32]);
        let credential =
            AuthCredential { day, credential: receive_auth_credential(&zkgroup::serialize(&issued), aci(), day, &public).unwrap() };
        // (Presenting with the made-up server's params instead of Signal's.)
        let stored: CallLinkAuthCredential = zkgroup::deserialize(&credential.credential).unwrap();
        let presentation =
            stored.present(aci(), Timestamp::from_epoch_seconds(day), &public, &CallLinkSecretParams::derive_from_root_key(&root_key), [3; 32]);
        presentation.verify(Timestamp::from_epoch_seconds(day + 60), &server, &link_public).unwrap();
        // Someone else's credential doesn't come through.
        assert!(receive_auth_credential(&zkgroup::serialize(&issued), Aci::from_uuid_bytes([8; 16]), day, &public).is_err());

        let room_id = [4u8; 32];
        let context = CreateCallLinkCredentialRequestContext::new(&room_id, [5; 32]);
        let response = context.get_request().issue(aci(), Timestamp::from_epoch_seconds(day), &server, [6; 32]);
        let auth = present_create(context, &zkgroup::serialize(&response), &room_id, aci(), &root_key, &public).unwrap();
        let presented: zkgroup::call_links::CreateCallLinkCredentialPresentation = zkgroup::deserialize(&auth.presentation).unwrap();
        presented.verify(&room_id, Timestamp::from_epoch_seconds(day + 60), &server, &link_public).unwrap();
        assert_eq!(auth.public_params, zkgroup::serialize(&link_public));
    }
}
