//! Nothing here reaches a server: requests go to [`Fake`], which answers
//! from a script. The account data and tree head below are libsignal's own
//! test vectors (`rust/net/chat/src/api/keytrans.rs`, its staging test
//! account, whose number is the 555 example `+18005550100`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::FutureExt as _;
use libsignal_keytrans::{ChatDistinguishedResponse, CondensedTreeSearchResponse, FullTreeHead, TreeHead};
use presage::libsignal_service::protocol::IdentityKeyPair;

use super::*;

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap()
}

const TEST_ACI: &str = "90c979fd-eab4-4a08-b6da-69dedeab9b29";
const TEST_IDENTITY_KEY: &str = "05cdcbb178067f0ddfd258bb21d006e0aa9c7ab132d9fb5e8b027de07d947f9d0c";
const TEST_USERNAME_HASH: &str = "dc711808c2cf66d5e6a33ce41f27d69d942d2e1ff4db22d39b42d2eff8d09746";
/// libsignal's `STORED_ACCOUNT_DATA_681`: ACI, number and username hash.
const STORED_ACCOUNT_DATA: &str = "0a3e0a203901c94081c4e6321e92b3e434dcaf788f5326913e7bdcab47b4fd2ae7a6848a10131a0308ff0320012a116190c979fdeab44a08b6da69dedeab9b29123a0a2086052cc2a2689558e852d053c5ab411d8c3baef20171ec298e551574806ca95d104c1a0308ff0320012a0d6e2b31383030353535303130301a4f0a206f55ac745ebeaf39fd5b21177c7fa692876c42678ffa1f3d58ed8fbc5ef023b910a6011a0308ff0320012a2175dc711808c2cf66d5e6a33ce41f27d69d942d2e1ff4db22d39b42d2eff8d0974622e8020abc0208a90510bec5b0c4d7331a640a20bd1e26a0fbdbfa923486ccc9296f4227db490b4add29f5507775171ea0fb7a4e124000eec714425c086bcde66e5a2af2c87031eea6c485965b549ac81bc3793cf5bdd5222073f59dd4959f6e9fdb6f6d405a477a7aaeb4bc8fd89b876b59cf89bf031a640a201123b13ee32479ae6af5739e5d687b51559abf7684120511f68cde7a21a0e755124034a1a53ac786258ad28d79b3b9cd9a59219b1449b10e769f3934d8981bf58d47c594439a93a3bb1adc318b5b227a60257406052f4501540d06c3d01b07a35a051a640a20093ee42d95502b3e81f4e604179c82c149fffb96167642b9eb81b03d6e2dd63612402cec2570d4fc3e3063aa9c358e2fcb55ae65f25f8c82072fc8d4016ce4fe2be182adeee08d786745109a8978189791feb52d2ebe7e98d1a22ab0478ea5190e0b1220032487becda242fa353ee8654ba3a5fb7d9e9aa1d48eb42f2ea6866070a494a118c4d6b0c4d733";
/// libsignal's `DISTINGUISHED_TREE_677_HEAD`, signed with staging's keys.
const STAGING_TREE_HEAD: &str = "08a50510f981afc4d7331a640a20bd1e26a0fbdbfa923486ccc9296f4227db490b4add29f5507775171ea0fb7a4e1240404fcd202d174fa3c1db70dd7a2af28aa7289230f16bbbabfbb0cf8ac0351ce8ddcc770a4e5ab2a2b32b4af7fba5e056f2d6f70be1039c152aeda2e7c6117a0d1a640a201123b13ee32479ae6af5739e5d687b51559abf7684120511f68cde7a21a0e7551240521e691c9356343feee2e80c5355d1f257550d870542ac0b6e25d349b6223966eb0859dd0df942cd5e541b37e9028682c9c986a5c9f33ce4739e205f58cbd2061a640a20093ee42d95502b3e81f4e604179c82c149fffb96167642b9eb81b03d6e2dd636124081185aaf680e96e329dee42cdb2f1ce7bef6da769b51dabfda8db0163977d500a47d00fe60a6fe3e2562f08e5ff8c4ec4dfcf054e31a85b28d0665ff92c0f901";

fn test_aci() -> Aci {
    Aci::parse_from_service_id_string(TEST_ACI).unwrap()
}

fn test_key() -> PublicKey {
    PublicKey::deserialize(&unhex(TEST_IDENTITY_KEY)).unwrap()
}

fn test_e164() -> E164 {
    "+18005550100".parse().unwrap()
}

fn test_account_data() -> AccountData {
    AccountData::try_from(StoredAccountData::decode(unhex(STORED_ACCOUNT_DATA).as_slice()).unwrap()).unwrap()
}

fn json(request: &WebSocketRequestMessage) -> serde_json::Value {
    serde_json::from_slice(request.body.as_deref().unwrap()).unwrap()
}

fn response(status: u32, headers: &[&str], body: Option<&[u8]>) -> WebSocketResponseMessage {
    WebSocketResponseMessage {
        id: None,
        status: Some(status),
        message: None,
        headers: headers.iter().map(|h| h.to_string()).collect(),
        body: body.map(<[u8]>::to_vec),
    }
}

fn ok_response(proto: &[u8]) -> WebSocketResponseMessage {
    let body = serde_json::json!({ "serializedResponse": BASE64_STANDARD_NO_PAD.encode(proto) });
    response(200, &["Content-Type:application/json"], Some(&serde_json::to_vec(&body).unwrap()))
}

#[test]
fn search_requests_are_what_libsignal_sends() {
    let key = test_key();
    let e164 = (test_e164(), vec![0x10; 16]);
    let hash = UsernameHash::new(unhex(TEST_USERNAME_HASH));
    let request = search_request(&test_aci(), &key, Some(&e164), Some(&hash), Some(681), 677);
    assert_eq!(request.verb.as_deref(), Some("POST"));
    assert_eq!(request.path.as_deref(), Some("/v1/key-transparency/search"));
    assert_eq!(request.headers, ["content-type:application/json", "accept:application/json"]);
    assert_eq!(
        json(&request),
        serde_json::json!({
            "aci": TEST_ACI,
            "aciIdentityKey": BASE64_STANDARD.encode(unhex(TEST_IDENTITY_KEY)),
            "e164": "+18005550100",
            // URL-safe, unpadded.
            "usernameHash": "3HEYCMLPZtXmozzkHyfWnZQtLh_02yLTm0LS7_jQl0Y",
            "unidentifiedAccessKey": "EBAQEBAQEBAQEBAQEBAQEA==",
            "lastTreeHeadSize": 681,
            "distinguishedTreeHeadSize": 677,
        })
    );
    // Only the ACI when that's all we know, and no last size the first time.
    let bare = search_request(&test_aci(), &key, None, None, None, 677);
    let body = json(&bare);
    let mut keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["aci", "aciIdentityKey", "distinguishedTreeHeadSize"]);
}

#[test]
fn distinguished_requests_are_what_libsignal_sends() {
    let first = distinguished_request(None);
    assert_eq!((first.verb.as_deref(), first.path.as_deref()), (Some("GET"), Some("/v1/key-transparency/distinguished?")));
    assert!(first.body.is_none());
    let next = distinguished_request(Some(677));
    assert_eq!(next.path.as_deref(), Some("/v1/key-transparency/distinguished?lastTreeHeadSize=677"));
}

#[test]
fn monitor_requests_are_what_libsignal_sends() {
    let data = test_account_data();
    let hash = UsernameHash::new(unhex(TEST_USERNAME_HASH));
    let request = monitor_request(&test_aci(), Some(&test_e164()), Some(&hash), &data, 677).unwrap();
    assert_eq!(request.path.as_deref(), Some("/v1/key-transparency/monitor"));
    let body = json(&request);
    assert_eq!(body["aci"]["value"], TEST_ACI);
    assert_eq!(body["aci"]["entryPosition"], data.aci.latest_log_position());
    assert_eq!(body["aci"]["commitmentIndex"], BASE64_STANDARD_NO_PAD.encode(data.aci.index.as_slice()));
    assert_eq!(body["e164"]["value"], "+18005550100");
    assert_eq!(body["usernameHash"]["value"], "3HEYCMLPZtXmozzkHyfWnZQtLh_02yLTm0LS7_jQl0Y");
    assert_eq!(body["lastNonDistinguishedTreeHeadSize"], 681);
    assert_eq!(body["lastDistinguishedTreeHeadSize"], 677);
    // Asking about a number the stored data doesn't have is a mistake.
    let mut without_number = data.clone();
    without_number.e164 = None;
    assert!(monitor_request(&test_aci(), Some(&test_e164()), Some(&hash), &without_number, 677).is_err());
}

#[test]
fn responses_are_read_as_libsignal_reads_them() {
    assert_eq!(serialized_response(&ok_response(b"proto")).unwrap(), b"proto");
    let failure = |r: WebSocketResponseMessage| Failure::from(serialized_response(&r).unwrap_err());
    assert_eq!(failure(response(429, &["Retry-After: 30"], None)), Failure::RateLimited(Duration::from_secs(30)));
    assert!(matches!(failure(response(429, &[], None)), Failure::Other(_)), "a 429 without a wait");
    assert!(matches!(failure(response(503, &[], None)), Failure::Other(_)));
    assert!(matches!(failure(response(403, &[], None)), Failure::Other(e) if e.contains("403")));
    assert!(matches!(failure(response(200, &[], Some(b"{}"))), Failure::Other(_)), "not JSON by its type");
    assert!(matches!(
        failure(response(200, &["content-type:application/json"], Some(br#"{"serializedResponse":"!"}"#))),
        Failure::KeyTransparency(_)
    ));
}

#[test]
fn failures_are_classed_as_desktop_classes_them() {
    let verification = RequestError::Other(kt::Error::VerificationFailed(libsignal_keytrans::Error::VerificationFailed(
        "bad signature".into(),
    )));
    assert!(matches!(Failure::from(verification), Failure::VerificationFailed(_)));
    let bad_data = RequestError::Other(kt::Error::VerificationFailed(libsignal_keytrans::Error::BadData("x".into())));
    assert!(matches!(Failure::from(bad_data), Failure::KeyTransparency(_)));
    assert!(matches!(Failure::from(RequestError::Timeout), Failure::Network(_)));
    assert!(matches!(Failure::from(RequestError::Disconnected(DisconnectedError::Closed)), Failure::Network(_)));
    assert!(matches!(Failure::from(RequestError::ServerSideError), Failure::Other(_)));
}

#[test]
fn self_health_moves_as_desktops() {
    use Health::*;
    let verification = Failure::VerificationFailed(String::new());
    let network = Failure::Network(String::new());
    let other = Failure::Other(String::new());
    let unrecognized = Failure::Unrecognized(String::new());
    // Verification failing: intermittent the first time, then failed.
    assert_eq!(next_health(None, &verification), Some(Intermittent));
    assert_eq!(next_health(Some(Ok), &verification), Some(Intermittent));
    assert_eq!(next_health(Some(Intermittent), &verification), Some(Fail));
    assert_eq!(next_health(Some(Fail), &verification), Some(Fail));
    // Passing trouble keeps a failed state, and otherwise forgets.
    assert_eq!(next_health(Some(Ok), &network), None);
    assert_eq!(next_health(Some(Intermittent), &network), Some(Intermittent));
    assert_eq!(next_health(Some(Fail), &Failure::RateLimited(Duration::ZERO)), Some(Fail));
    assert_eq!(next_health(Some(Fail), &Failure::KeyTransparency(String::new())), Some(Fail));
    // Not libsignal's: unknown. A libsignal error Desktop doesn't know: failed.
    assert_eq!(next_health(Some(Fail), &other), None);
    assert_eq!(next_health(Some(Ok), &unrecognized), Some(Fail));
}

#[test]
fn the_self_check_is_scheduled_as_desktops() {
    let week = SELF_CHECK_INTERVAL.as_millis() as i64;
    let now = 1_000 * week;
    // A week after the last one, give or take 5%.
    assert_eq!(next_self_check(Some(now), now, 0.3, 0.5), now + week);
    assert_eq!(next_self_check(Some(now), now, 0.3, 0.), now + week * 95 / 100);
    // Never run: somewhere in the coming week.
    let first = next_self_check(None, now, 0.25, 0.5);
    assert_eq!(first, now + week * 3 / 4);
    // `runAt` and `delayBy`.
    assert_eq!(next_self_check(Some(last_for_run_at(now + 300_000)), now, 0., 0.5), now + 300_000);
    let delay = KNOWN_IDENTIFIER_CHANGE_DELAY;
    assert_eq!(last_for_delay(Some(now), now, delay), now, "a check a week off isn't brought forward");
    let overdue = now - week;
    assert_eq!(next_self_check(Some(last_for_delay(Some(overdue), now, delay)), now, 0., 0.5), now + 300_000);
}

/// A server that answers from a script, recording what it was asked.
#[derive(Clone, Default)]
struct Fake {
    answers: Arc<Mutex<VecDeque<Result<WebSocketResponseMessage, DisconnectedError>>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn answering(answers: impl IntoIterator<Item = Result<WebSocketResponseMessage, DisconnectedError>>) -> Self {
        Self { answers: Arc::new(Mutex::new(answers.into_iter().collect())), ..Default::default() }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Transport for Fake {
    fn send(
        &self,
        request: WebSocketRequestMessage,
    ) -> BoxFuture<'static, Result<WebSocketResponseMessage, DisconnectedError>> {
        self.asked.lock().unwrap().push(request.path.unwrap_or_default());
        let answer = self.answers.lock().unwrap().pop_front().unwrap_or(Err(DisconnectedError::Closed));
        async move { answer }.boxed()
    }
}

fn contact_request() -> Request {
    Request {
        aci: test_aci(),
        identity_key: test_key(),
        e164: Some((test_e164(), [0x10; 16])),
        username_hash: None,
        mode: CheckMode::ContactCheck,
    }
}

async fn stored(db: &AppDb) -> (Option<String>, Option<String>) {
    let account = db.setting(&account_key(&test_aci())).await.unwrap();
    (account, db.setting(SETTING_DISTINGUISHED).await.unwrap())
}

/// Records each wait instead of waiting.
fn recording_sleep() -> (Arc<Mutex<Vec<u64>>>, impl Fn(Duration) -> BoxFuture<'static, ()>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let record = waits.clone();
    (waits, move |wait: Duration| {
        record.lock().unwrap().push(wait.as_secs());
        async {}.boxed()
    })
}

#[tokio::test]
async fn no_network_is_tried_with_desktops_back_off() {
    let db = AppDb::open_in_memory().await.unwrap();
    let fake = Fake::default();
    let (waits, sleep) = recording_sleep();
    let result = check_with(&db, &ChatApi(fake.clone()), &contact_request(), sleep).await;
    assert!(matches!(result, Err(Failure::Network(_))), "{result:?}");
    // Six rounds of three tries a second apart, with 5, 8, 13, 21 and 34
    // seconds between rounds.
    assert_eq!(fake.asked().len(), 18);
    assert!(fake.asked().iter().all(|p| p.starts_with("/v1/key-transparency/distinguished")));
    let round = [1, 1];
    let expected: Vec<u64> = [5, 8, 13, 21, 34].iter().flat_map(|b| round.iter().copied().chain([*b])).chain(round).collect();
    assert_eq!(*waits.lock().unwrap(), expected);
    assert_eq!(stored(&db).await, (None, None));
}

#[tokio::test]
async fn a_rate_limit_waits_as_long_as_the_server_says() {
    let db = AppDb::open_in_memory().await.unwrap();
    let fake = Fake::answering([Ok(response(429, &["Retry-After:7"], None)), Ok(response(500, &[], None))]);
    let (waits, sleep) = recording_sleep();
    let result = check_with(&db, &ChatApi(fake.clone()), &contact_request(), sleep).await;
    // A server error isn't retried.
    assert!(matches!(result, Err(Failure::Other(_))), "{result:?}");
    assert_eq!(fake.asked().len(), 2);
    assert_eq!(*waits.lock().unwrap(), [7]);
}

#[tokio::test]
async fn a_forged_tree_head_is_refused_and_nothing_is_kept() {
    let db = AppDb::open_in_memory().await.unwrap();
    // A real tree head, but signed by staging's keys, not production's.
    let forged = ChatDistinguishedResponse {
        tree_head: Some(FullTreeHead {
            tree_head: Some(TreeHead::decode(unhex(STAGING_TREE_HEAD).as_slice()).unwrap()),
            ..Default::default()
        }),
        distinguished: Some(CondensedTreeSearchResponse::default()),
    };
    let fake = Fake::answering([Ok(ok_response(&forged.encode_to_vec()))]);
    let result = check(&db, &ChatApi(fake.clone()), &contact_request()).await;
    assert!(matches!(result, Err(Failure::VerificationFailed(_))), "{result:?}");
    assert_eq!(fake.asked().len(), 1, "not retried");
    assert_eq!(stored(&db).await, (None, None));
    // Nor is garbage.
    let fake = Fake::answering([Ok(ok_response(b"\xff\xff"))]);
    let result = check(&db, &ChatApi(fake), &contact_request()).await;
    assert!(matches!(result, Err(Failure::KeyTransparency(_))), "{result:?}");
}

#[tokio::test]
async fn stored_data_is_reset_imported_and_forgotten() {
    let db = AppDb::open_in_memory().await.unwrap();
    let aci = test_aci();
    import_account_data(&db, &aci, &unhex(STORED_ACCOUNT_DATA)).await.unwrap();
    reset_field(&db, &aci, AccountDataField::UsernameHash).await.unwrap();
    let bytes = load_bytes(&db, &account_key(&aci)).await.unwrap().unwrap();
    let data = StoredAccountData::decode(bytes.as_slice()).unwrap();
    assert!(data.username_hash.is_none());
    assert!(data.e164.is_some(), "the rest stays");
    // Nothing stored, nothing to reset.
    let other = Aci::from_uuid_bytes([7; 16]);
    reset_field(&db, &other, AccountDataField::E164).await.unwrap();
    assert_eq!(db.setting(&account_key(&other)).await.unwrap(), None);

    set_self_health(&db, Some(Health::Intermittent)).await.unwrap();
    assert_eq!(self_health(&db).await, Some(Health::Intermittent));
    db.set_setting(SETTING_LAST_SELF_CHECK, "5").await.unwrap();
    db.set_setting(SETTING_DISTINGUISHED, "AA==").await.unwrap();
    forget_all(&db).await.unwrap();
    assert_eq!(stored(&db).await, (None, None));
    assert_eq!(self_health(&db).await, None);
    assert_eq!(db.setting(SETTING_LAST_SELF_CHECK).await.unwrap(), None);
}

#[test]
fn identity_keys_from_presage_are_libsignals() {
    // The check takes presage's identity keys as they are.
    let pair = IdentityKeyPair::generate(&mut rand::rng());
    let key: &PublicKey = pair.identity_key().public_key();
    assert_eq!(key.serialize().len(), 33);
}
