//! Key transparency: Desktop's "Automatic key verification" in the safety
//! number screen and its weekly check of our own account
//! (`ts/services/keyTransparency.preload.ts`), using libsignal's client and
//! verifier (`libsignal_net_chat::api::keytrans::check`, called as
//! libsignal's own bridge `KeyTransparency_Check` calls it).
//!
//! libsignal sends key transparency requests on libsignal-net's own
//! unauthenticated chat connection, which we don't have: presage talks to
//! the chat server through libsignal-service-rs. [`ChatApi`] implements
//! libsignal's `LowLevelChatApi` (its three requests) over presage's
//! unauthenticated websocket instead, building the requests libsignal's
//! `rust/net/chat/src/ws/keytrans.rs` builds and reading the responses as it
//! does.
//!
//! What Desktop keeps, we keep in settings rows: each account's verified
//! data (Desktop's `key_transparency_account_data` table), the last
//! distinguished tree head (`lastDistinguishedTreeHead`), the self check's
//! health (`keyTransparencySelfHealth`) and when it last ran
//! (`lastKeyTransparencySelfCheck`).

use std::fmt;
use std::time::{Duration, SystemTime};

use base64::prelude::{BASE64_STANDARD, BASE64_STANDARD_NO_PAD, BASE64_URL_SAFE_NO_PAD, Engine as _};
use futures::future::BoxFuture;
use libsignal_keytrans::{AccountData, LastTreeHead, StoredAccountData, StoredTreeHead};
use libsignal_net::infra::errors::RetryLater;
use libsignal_net_chat::api::keytrans::{
    self as kt, AccountDataField, AccountDataFieldReset as _, CheckMode, KeyTransparencyClient, LowLevelChatApi,
    SearchKey as _, TreeHeadWithTimestamp, UsernameHash,
};
use libsignal_net_chat::api::{DisconnectedError, RequestError};
use presage::libsignal_service::protocol::{Aci, E164, PublicKey};
use presage::libsignal_service::websocket::{SignalWebSocket, Unidentified};
use presage::proto::{WebSocketRequestMessage, WebSocketResponseMessage};
use prost::Message as _;
use signal_store::AppDb;

/// Desktop's `keyTransparency` remote config flag
/// (`desktop.keyTransparency.prod`, read as `isKeyTransparencyAvailable`
/// reads it).
pub fn feature_on() -> bool {
    crate::remote_config().feature("keyTransparency")
}

/// Settings key: `"true"` when automatic key verification is off (Desktop's
/// `hasKeyTransparencyDisabled`, the account record's
/// `automaticKeyVerificationDisabled`).
pub const SETTING_DISABLED: &str = "key_transparency.disabled";
/// Settings key: `"true"` once Desktop's "Signal can now auto-verify
/// encryption" dialog was seen (`hasSeenKeyTransparencyOnboarding`).
pub const SETTING_ONBOARDING_SEEN: &str = "key_transparency.onboarding_seen";
/// The last distinguished tree head, a `StoredTreeHead` (base64).
const SETTING_DISTINGUISHED: &str = "key_transparency.distinguished";
/// `key_transparency.account.<ACI>`: their verified account data, a
/// `StoredAccountData` (base64).
const SETTING_ACCOUNT: &str = "key_transparency.account.";
/// The self check's health: `ok`, `intermittent` or `fail`; unset: unknown.
const SETTING_SELF_HEALTH: &str = "key_transparency.self_health";
/// When the self check last ran, as Desktop's `CheckScheduler` keeps it
/// (unix millis; moved to schedule the next run).
pub(crate) const SETTING_LAST_SELF_CHECK: &str = "key_transparency.last_self_check";
/// Settings key: `"true"` when our phone number is discoverable (Desktop's
/// `phoneNumberDiscoverability`, from the account record's
/// `unlistedPhoneNumber`). Unset counts as not discoverable, as in Desktop.
pub(crate) const SETTING_E164_DISCOVERABLE: &str = "phone_number_discoverable";
/// Settings key: our username, from the account record.
pub(crate) const SETTING_USERNAME: &str = "account.username";

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often our own account is checked (Desktop's `CheckScheduler`
/// interval).
pub(crate) const SELF_CHECK_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// `KNOWN_IDENTIFIER_CHANGE_DELAY`: after our username, number, access key
/// or discoverability changes, the self check waits this long (the log
/// takes a while to catch up).
pub(crate) const KNOWN_IDENTIFIER_CHANGE_DELAY: Duration = Duration::from_secs(5 * 60);
/// `INTERMITTENT_ERROR_RETRY_DELAY`: a self check that failed once is
/// tried again after this long.
pub(crate) const INTERMITTENT_ERROR_RETRY_DELAY: Duration = DAY;
/// Desktop's `KEY_TRANSPARENCY_TIMEOUTS` (`FIBONACCI_TIMEOUTS.slice(3)`):
/// waits between tries after a network error, in seconds.
const BACKOFF_SECS: [u64; 6] = [5, 8, 13, 21, 34, 55];
/// Desktop's `_retry`: tries per request when there's no network.
const NETWORK_TRIES: usize = 3;
/// How long a request may take before it counts as a network error. Our
/// websocket has no request timeout of its own.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const SEARCH_PATH: &str = "/v1/key-transparency/search";
const DISTINGUISHED_PATH: &str = "/v1/key-transparency/distinguished";
const MONITOR_PATH: &str = "/v1/key-transparency/monitor";

/// Desktop's `keyTransparencySelfHealth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ok,
    /// Verification failed once; tried again in a day.
    Intermittent,
    Fail,
}

impl Health {
    fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Intermittent => "intermittent",
            Health::Fail => "fail",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "ok" => Some(Health::Ok),
            "intermittent" => Some(Health::Intermittent),
            "fail" => Some(Health::Fail),
            _ => None,
        }
    }
}

/// Why a check failed, in the classes Desktop tells apart (libsignal's
/// error codes as its Node bridge names them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// `KeyTransparencyVerificationFailed`: the log disagrees with what the
    /// server says (or with what we verified before).
    VerificationFailed(String),
    /// `KeyTransparencyError`: any other key transparency error, such as a
    /// response missing a field we asked for.
    KeyTransparency(String),
    /// `IoError` / `ChatServiceInactive`: no connection, or it dropped or
    /// timed out.
    Network(String),
    /// `RateLimitedError`, with the server's wait.
    RateLimited(Duration),
    /// A libsignal error Desktop has no case for (a rate limit challenge).
    Unrecognized(String),
    /// Not a libsignal error: an unexpected status, a server error, the
    /// check not being possible, or our own storage failing.
    Other(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::VerificationFailed(e) => write!(f, "verification failed: {e}"),
            Failure::KeyTransparency(e) => write!(f, "key transparency error: {e}"),
            Failure::Network(e) => write!(f, "network error: {e}"),
            Failure::RateLimited(wait) => write!(f, "rate limited for {}s", wait.as_secs()),
            Failure::Unrecognized(e) | Failure::Other(e) => f.write_str(e),
        }
    }
}

impl From<RequestError<kt::Error>> for Failure {
    fn from(error: RequestError<kt::Error>) -> Self {
        match error {
            // libsignal's Node bridge: only the verifier's own failure is
            // `KeyTransparencyVerificationFailed`.
            RequestError::Other(kt::Error::VerificationFailed(libsignal_keytrans::Error::VerificationFailed(e))) => {
                Failure::VerificationFailed(e)
            }
            RequestError::Other(e) => Failure::KeyTransparency(e.to_string()),
            RequestError::Timeout => Failure::Network("the request timed out".into()),
            RequestError::Disconnected(e) => Failure::Network(e.to_string()),
            RequestError::RetryLater(RetryLater { retry_after_seconds }) => {
                Failure::RateLimited(Duration::from_secs(retry_after_seconds.into()))
            }
            e @ RequestError::Challenge(_) => Failure::Unrecognized(e.to_string()),
            e @ (RequestError::ServerSideError | RequestError::Unexpected { .. }) => Failure::Other(e.to_string()),
        }
    }
}

fn store_failure(error: impl fmt::Display) -> Failure {
    Failure::Other(format!("storage: {error}"))
}

/// The self check's next health after `failure`, as Desktop's `#selfCheck`
/// decides it.
pub(crate) fn next_health(old: Option<Health>, failure: &Failure) -> Option<Health> {
    let failing = matches!(old, Some(Health::Intermittent | Health::Fail));
    match failure {
        // Once is intermittent; again (or after a failure) is a failure.
        Failure::VerificationFailed(_) => Some(if failing { Health::Fail } else { Health::Intermittent }),
        // Keep a failed state until a check succeeds; otherwise unknown.
        Failure::KeyTransparency(_) | Failure::Network(_) | Failure::RateLimited(_) => old.filter(|_| failing),
        Failure::Unrecognized(_) => Some(Health::Fail),
        // Not a libsignal error: Desktop leaves the state unknown.
        Failure::Other(_) => None,
    }
}

/// One key transparency check: whose, and what we know about them.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub aci: Aci,
    pub identity_key: PublicKey,
    /// Their phone number and unidentified access key (from their profile
    /// key), when we have both (Desktop's `e164Info`).
    pub e164: Option<(E164, [u8; 16])>,
    /// Our username's hash, for the self check.
    pub username_hash: Option<[u8; 32]>,
    pub mode: CheckMode,
}

// ------------------------------------------------------------------ store

fn account_key(aci: &Aci) -> String {
    format!("{SETTING_ACCOUNT}{}", aci.service_id_string())
}

async fn load_bytes(db: &AppDb, key: &str) -> Result<Option<Vec<u8>>, Failure> {
    let value = db.setting(key).await.map_err(store_failure)?;
    // A value that isn't base64 is as good as none.
    Ok(value.and_then(|v| BASE64_STANDARD.decode(v).ok()).filter(|b| !b.is_empty()))
}

async fn save_bytes(db: &AppDb, key: &str, bytes: &[u8]) -> Result<(), Failure> {
    db.set_setting(key, &BASE64_STANDARD.encode(bytes)).await.map_err(store_failure)
}

/// Their account data from a transferred backup (`Contact.keyTransparencyData`),
/// as Desktop's backup import keeps it.
pub(crate) async fn import_account_data(db: &AppDb, aci: &Aci, data: &[u8]) -> signal_store::Result<()> {
    db.set_setting(&account_key(aci), &BASE64_STANDARD.encode(data)).await
}

pub(crate) async fn self_health(db: &AppDb) -> Option<Health> {
    db.setting(SETTING_SELF_HEALTH).await.ok().flatten().as_deref().and_then(Health::parse)
}

pub(crate) async fn set_self_health(db: &AppDb, health: Option<Health>) -> signal_store::Result<()> {
    match health {
        Some(health) => db.set_setting(SETTING_SELF_HEALTH, health.as_str()).await,
        None => db.remove_setting(SETTING_SELF_HEALTH).await,
    }
}

pub(crate) async fn is_disabled(db: &AppDb) -> bool {
    db.setting(SETTING_DISABLED).await.ok().flatten().as_deref() == Some("true")
}

/// Desktop's `isKeyTransparencyAvailable`: the feature is on and the user
/// hasn't turned it off.
pub(crate) async fn is_available(db: &AppDb) -> bool {
    feature_on() && !is_disabled(db).await
}

/// Desktop's `KeyTransparency.disable`: forgets everything verified.
pub(crate) async fn forget_all(db: &AppDb) -> signal_store::Result<()> {
    db.remove_settings_with_prefix(SETTING_ACCOUNT).await?;
    db.remove_setting(SETTING_DISTINGUISHED).await?;
    db.remove_setting(SETTING_SELF_HEALTH).await?;
    db.remove_setting(SETTING_LAST_SELF_CHECK).await
}

/// libsignal's `resetField`: forgets one field of our stored account data
/// (after our number or username changed), keeping the rest.
pub(crate) async fn reset_field(db: &AppDb, aci: &Aci, field: AccountDataField) -> Result<(), Failure> {
    let key = account_key(aci);
    let Some(bytes) = load_bytes(db, &key).await? else { return Ok(()) };
    let stored = StoredAccountData::decode(bytes.as_slice())
        .map_err(|_| Failure::Other("failed to decode account data".into()))?;
    save_bytes(db, &key, &stored.reset(field).encode_to_vec()).await
}

// ------------------------------------------------------------------ check

/// One check, without retries: libsignal's `KeyTransparencyClient.check`
/// (`node/ts/net/KeyTransparency.ts`) and the bridge's
/// `KeyTransparency_Check` (`rust/bridge/shared/src/net/keytrans.rs`). Our
/// stored data is only replaced once the result verified in full.
async fn check_once(db: &AppDb, chat: &(dyn LowLevelChatApi + Sync), request: &Request) -> Result<(), Failure> {
    let key = account_key(&request.aci);
    let account_data = match load_bytes(db, &key).await? {
        None => None,
        Some(bytes) => {
            let stored = StoredAccountData::decode(bytes.as_slice()).map_err(|_| {
                Failure::from(RequestError::Other(kt::Error::InvalidRequest("could not decode account data")))
            })?;
            Some(AccountData::try_from(stored).map_err(|e| Failure::from(RequestError::Other(kt::Error::from(e))))?)
        }
    };
    let distinguished = load_bytes(db, SETTING_DISTINGUISHED)
        .await?
        .and_then(|bytes| StoredTreeHead::decode(bytes.as_slice()).ok())
        .and_then(TreeHeadWithTimestamp::from_stored);
    let previous = distinguished.as_ref().map(|d| d.tree_head.clone());

    let e164 = request.e164.map(|(e164, key)| (e164, key.to_vec()));
    let e164_search_key = request.e164.map(|(e164, _)| e164.as_search_key());
    let username_hash = request.username_hash.map(|hash| UsernameHash::new(hash.to_vec()));
    let hash_search_key = username_hash.as_ref().map(|hash| hash.as_search_key());

    let client = KeyTransparencyClient::new(chat, libsignal_net::env::PROD.keytrans_config);
    let (result, updated) = kt::check(
        &client,
        &request.aci,
        &request.identity_key,
        e164,
        username_hash,
        account_data,
        distinguished,
        request.mode,
    )
    .await?;

    let now = SystemTime::now();
    let account = result
        .map(|data| data.into_stored(request.aci.as_search_key(), e164_search_key, hash_search_key, now).encode_to_vec())
        .into_result()
        .map_err(|missing| {
            let missing: Vec<String> = missing.iter().map(|f| format!("{f:?}")).collect();
            Failure::from(RequestError::Other(kt::Error::InvalidResponse(format!(
                "Some fields are missing from the response: {}",
                missing.join(", ")
            ))))
        })?;
    save_bytes(db, &key, &account).await?;
    // A reused distinguished head keeps the time it was stored.
    if previous.as_ref() != Some(&updated) {
        save_bytes(db, SETTING_DISTINGUISHED, &updated.into_stored(now).encode_to_vec()).await?;
    }
    Ok(())
}

/// Desktop's `#check`: one check, tried again after network errors (three
/// times a second apart, as its `_retry`, then with its back-off) and after
/// a rate limit (when the server says). Anything else fails at once.
pub(crate) async fn check(db: &AppDb, chat: &(dyn LowLevelChatApi + Sync), request: &Request) -> Result<(), Failure> {
    check_with(db, chat, request, |wait| Box::pin(tokio::time::sleep(wait))).await
}

/// [`check`], waiting with `sleep`.
async fn check_with(
    db: &AppDb,
    chat: &(dyn LowLevelChatApi + Sync),
    request: &Request,
    sleep: impl Fn(Duration) -> BoxFuture<'static, ()>,
) -> Result<(), Failure> {
    let mut backoff = 0;
    loop {
        let mut tries = 0;
        let failure = loop {
            tries += 1;
            match check_once(db, chat, request).await {
                Ok(()) => return Ok(()),
                Err(Failure::Network(_)) if tries < NETWORK_TRIES => sleep(Duration::from_secs(1)).await,
                Err(failure) => break failure,
            }
        };
        // `BackOff.isFull`: the last wait isn't used.
        if backoff == BACKOFF_SECS.len() - 1 {
            return Err(failure);
        }
        let wait = match &failure {
            Failure::Network(_) => Duration::from_secs(BACKOFF_SECS[backoff]),
            Failure::RateLimited(wait) => *wait,
            _ => return Err(failure),
        };
        backoff += 1;
        tracing::warn!(%failure, wait_secs = wait.as_secs(), "key transparency: retrying");
        sleep(wait).await;
    }
}

// ------------------------------------------------------------ the requests

/// Sends a request on the chat server's unauthenticated websocket.
pub(crate) trait Transport: Send + Sync {
    fn send(
        &self,
        request: WebSocketRequestMessage,
    ) -> BoxFuture<'static, Result<WebSocketResponseMessage, DisconnectedError>>;
}

/// presage's unauthenticated websocket.
pub(crate) struct Websocket(std::sync::Mutex<SignalWebSocket<Unidentified>>);

impl Websocket {
    pub(crate) fn new(ws: SignalWebSocket<Unidentified>) -> Self {
        Self(std::sync::Mutex::new(ws))
    }
}

impl Transport for Websocket {
    fn send(
        &self,
        request: WebSocketRequestMessage,
    ) -> BoxFuture<'static, Result<WebSocketResponseMessage, DisconnectedError>> {
        let mut ws = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Box::pin(async move {
            ws.request(request).await.map_err(|error| match error {
                presage::libsignal_service::prelude::ServiceError::WsClosing { .. } => DisconnectedError::Closed,
                error => DisconnectedError::Transport { log_safe: error.to_string() },
            })
        })
    }
}

/// libsignal's key transparency requests over a [`Transport`].
pub(crate) struct ChatApi<T>(pub T);

impl<T: Transport> ChatApi<T> {
    async fn send_kt(&self, request: WebSocketRequestMessage) -> Result<Vec<u8>, RequestError<kt::Error>> {
        tracing::debug!(path = request.path.as_deref().unwrap_or_default(), "key transparency request");
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, self.0.send(request)).await {
            Err(_) => return Err(RequestError::Timeout),
            Ok(Err(error)) => return Err(RequestError::Disconnected(error)),
            Ok(Ok(response)) => response,
        };
        serialized_response(&response)
    }
}

fn header<'a>(response: &'a WebSocketResponseMessage, name: &str) -> Option<&'a str> {
    response.headers.iter().find_map(|h| {
        let (key, value) = h.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

/// A key transparency response's protobuf bytes, or the error libsignal's
/// `send_kt_request` makes of it (`TryIntoResponse` and
/// `into_request_error` in `rust/net/chat/src/ws.rs`).
pub(crate) fn serialized_response(response: &WebSocketResponseMessage) -> Result<Vec<u8>, RequestError<kt::Error>> {
    let status = response.status.unwrap_or_default();
    if !(200..300).contains(&status) {
        if (500..600).contains(&status) {
            return Err(RequestError::ServerSideError);
        }
        if status == 429
            && let Some(retry_after_seconds) = header(response, "retry-after").and_then(|v| v.parse().ok())
        {
            return Err(RequestError::RetryLater(RetryLater { retry_after_seconds }));
        }
        if status == 422 {
            return Err(RequestError::Unexpected { log_safe: "the request did not pass server validation".into() });
        }
        return Err(RequestError::Unexpected { log_safe: format!("unexpected response status {status}") });
    }
    let content_type = header(response, "content-type");
    if content_type.is_none_or(|t| !t.eq_ignore_ascii_case("application/json")) {
        return Err(RequestError::Unexpected { log_safe: format!("unexpected content type {content_type:?}") });
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Serialized {
        serialized_response: String,
    }
    let body = response.body.as_deref().ok_or(RequestError::Unexpected { log_safe: "missing body".into() })?;
    let parsed: Serialized = serde_json::from_slice(body)
        .map_err(|_| RequestError::Unexpected { log_safe: "unexpected response body".into() })?;
    BASE64_STANDARD_NO_PAD
        .decode(&parsed.serialized_response)
        .map_err(|_| RequestError::Other(kt::Error::InvalidResponse("invalid base64".into())))
}

/// A JSON request as libsignal sends them (`common_headers`: JSON in and
/// out, even for the GET).
fn json_request(verb: &str, path: String, body: Option<serde_json::Value>) -> WebSocketRequestMessage {
    WebSocketRequestMessage {
        verb: Some(verb.into()),
        path: Some(path),
        body: body.map(|b| serde_json::to_vec(&b).expect("JSON")),
        headers: vec!["content-type:application/json".into(), "accept:application/json".into()],
        id: None,
    }
}

/// libsignal's `RawChatSearchRequest`.
pub(crate) fn search_request(
    aci: &Aci,
    identity_key: &PublicKey,
    e164: Option<&(E164, Vec<u8>)>,
    username_hash: Option<&UsernameHash<'_>>,
    last_tree_head_size: Option<u64>,
    distinguished_tree_head_size: u64,
) -> WebSocketRequestMessage {
    let mut body = serde_json::Map::new();
    body.insert("aci".into(), aci.service_id_string().into());
    body.insert("aciIdentityKey".into(), BASE64_STANDARD.encode(identity_key.serialize()).into());
    if let Some((e164, _)) = e164 {
        body.insert("e164".into(), e164.to_string().into());
    }
    if let Some(hash) = username_hash {
        body.insert("usernameHash".into(), BASE64_URL_SAFE_NO_PAD.encode(hash.as_ref()).into());
    }
    if let Some((_, access_key)) = e164 {
        body.insert("unidentifiedAccessKey".into(), BASE64_STANDARD.encode(access_key).into());
    }
    if let Some(size) = last_tree_head_size {
        body.insert("lastTreeHeadSize".into(), size.into());
    }
    body.insert("distinguishedTreeHeadSize".into(), distinguished_tree_head_size.into());
    json_request("POST", SEARCH_PATH.into(), Some(body.into()))
}

/// libsignal's `RawChatDistinguishedRequest` (its path keeps the `?` when
/// there's no size).
pub(crate) fn distinguished_request(last_tree_head_size: Option<u64>) -> WebSocketRequestMessage {
    let query = last_tree_head_size.map(|n| format!("lastTreeHeadSize={n}")).unwrap_or_default();
    json_request("GET", format!("{DISTINGUISHED_PATH}?{query}"), None)
}

fn value_monitor(value: String, entry_position: u64, commitment_index: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "value": value,
        "entryPosition": entry_position,
        "commitmentIndex": BASE64_STANDARD_NO_PAD.encode(commitment_index),
    })
}

/// libsignal's `RawChatMonitorRequest`.
pub(crate) fn monitor_request(
    aci: &Aci,
    e164: Option<&E164>,
    username_hash: Option<&UsernameHash<'_>>,
    account_data: &AccountData,
    distinguished_tree_head_size: u64,
) -> Result<WebSocketRequestMessage, kt::Error> {
    if e164.is_some() != account_data.e164.is_some() || username_hash.is_some() != account_data.username_hash.is_some() {
        return Err(kt::Error::InvalidRequest("account data does not match the monitor request"));
    }
    let mut body = serde_json::Map::new();
    let data = &account_data.aci;
    body.insert("aci".into(), value_monitor(aci.service_id_string(), data.latest_log_position(), &data.index));
    if let (Some(e164), Some(data)) = (e164, &account_data.e164) {
        body.insert("e164".into(), value_monitor(e164.to_string(), data.latest_log_position(), &data.index));
    }
    if let (Some(hash), Some(data)) = (username_hash, &account_data.username_hash) {
        let value = BASE64_URL_SAFE_NO_PAD.encode(hash.as_ref());
        body.insert("usernameHash".into(), value_monitor(value, data.latest_log_position(), &data.index));
    }
    body.insert("lastNonDistinguishedTreeHeadSize".into(), account_data.last_tree_head.0.tree_size.into());
    body.insert("lastDistinguishedTreeHeadSize".into(), distinguished_tree_head_size.into());
    Ok(json_request("POST", MONITOR_PATH.into(), Some(body.into())))
}

#[async_trait::async_trait]
impl<T: Transport> LowLevelChatApi for ChatApi<T> {
    async fn search(
        &self,
        aci: &Aci,
        aci_identity_key: &PublicKey,
        e164: Option<&(E164, Vec<u8>)>,
        username_hash: Option<&UsernameHash<'_>>,
        stored_account_data: Option<&AccountData>,
        distinguished_tree_head: &LastTreeHead,
    ) -> Result<Vec<u8>, RequestError<kt::Error>> {
        let request = search_request(
            aci,
            aci_identity_key,
            e164,
            username_hash,
            stored_account_data.map(|data| data.last_tree_head.0.tree_size),
            distinguished_tree_head.0.tree_size,
        );
        self.send_kt(request).await
    }

    async fn distinguished(&self, last_distinguished: Option<&LastTreeHead>) -> Result<Vec<u8>, RequestError<kt::Error>> {
        self.send_kt(distinguished_request(last_distinguished.map(|head| head.0.tree_size))).await
    }

    async fn monitor(
        &self,
        aci: &Aci,
        e164: Option<&E164>,
        username_hash: Option<&UsernameHash<'_>>,
        account_data: &AccountData,
        last_distinguished_tree_head: &LastTreeHead,
    ) -> Result<Vec<u8>, RequestError<kt::Error>> {
        let request =
            monitor_request(aci, e164, username_hash, account_data, last_distinguished_tree_head.0.tree_size)
                .map_err(RequestError::Other)?;
        self.send_kt(request).await
    }
}

// ------------------------------------------------------------- scheduling

/// When the next self check is due, as Desktop's `CheckScheduler` works it
/// out: a week after the last, give or take 5%. Without a last one, a
/// random time within the week (Desktop's gradual rollout). `rollout` and
/// `jitter` are random numbers in `[0, 1)`.
pub(crate) fn next_self_check(last: Option<i64>, now: i64, rollout: f64, jitter: f64) -> i64 {
    let interval = SELF_CHECK_INTERVAL.as_millis() as f64;
    let last = last.unwrap_or_else(|| now - (interval * rollout) as i64);
    last + (interval * (1. + (jitter * 0.1 - 0.05))) as i64
}

/// The stored "last check" that makes the next one due at `at`
/// (`CheckScheduler.runAt`).
pub(crate) fn last_for_run_at(at: i64) -> i64 {
    at - SELF_CHECK_INTERVAL.as_millis() as i64
}

/// `CheckScheduler.delayBy`: the stored "last check" after putting the next
/// one off until at least `delay` from now.
pub(crate) fn last_for_delay(last: Option<i64>, now: i64, delay: Duration) -> i64 {
    last.unwrap_or(0).max(now + delay.as_millis() as i64 - SELF_CHECK_INTERVAL.as_millis() as i64)
}

#[cfg(test)]
mod tests;
