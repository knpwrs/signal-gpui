//! Sender key sends and group send endorsements: the wire format (checked
//! against libsignal-net and libsignal's own parser and decrypter), the
//! endorsement tokens (checked with a made-up server's keys, as the chat
//! server checks them), and every way the send loop starts over or falls
//! over (against a fake network). Nothing here talks to a server.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::SystemTime;

use presage::libsignal_service::groups_v2::endorsements::GroupSendEndorsements;
use presage::libsignal_service::protocol::{
    Aci, CiphertextMessageType, ContentHint, DeviceId, GenericSignedPreKey, IdentityKeyPair, IdentityKeyStore,
    InMemSignalProtocolStore, KeyPair, KyberPreKeyRecord, PreKeyBundle, PreKeyRecord, PreKeyStore, ProtocolAddress,
    SealedSenderV2SentMessage, SenderCertificate, SenderKeyDistributionMessage, ServerCertificate, ServiceId,
    SignedPreKeyRecord, SignedPreKeyStore, KyberPreKeyStore, Timestamp, UnidentifiedSenderMessageContent,
    create_sender_key_distribution_message, group_decrypt, group_encrypt, kem, process_prekey_bundle,
    process_sender_key_distribution_message, sealed_sender_decrypt_to_usmc,
};
use presage::libsignal_service::push_service::{
    AccountMismatchedDevices, AccountStaleDevices, MismatchedDevices, ServiceError, StaleDevices,
};
use presage::libsignal_service::sender::MessageSenderError;
use presage::libsignal_service::sender::sender_key::{
    MAX_ATTEMPTS, SENDER_KEY_MAX_AGE_MS, GroupSendAuth, SenderKeyDevice, SenderKeyInfo, SenderKeyOutcome,
    SenderKeyRequest, SenderKeyTransport, analyze_sender_key_devices, multi_recipient_payload, send_via_sender_key,
};
use presage::libsignal_service::websocket::sender::{
    MULTI_RECIPIENT_CONTENT_TYPE, MultiRecipientAuth, multi_recipient_request,
};
use presage::libsignal_service::zkgroup::groups::{
    GroupSecretParams, GroupSendDerivedKeyPair, GroupSendEndorsementsResponse, GroupSendFullToken,
};
use presage::libsignal_service::zkgroup::{self, SECONDS_PER_DAY, ServerSecretParams};
use rand::Rng as _;
use uuid::Uuid;

use super::*;

fn aci(n: u8) -> Aci {
    Aci::from_uuid_bytes([n; 16])
}

fn sid(n: u8) -> ServiceId {
    aci(n).into()
}

fn device(n: u8) -> DeviceId {
    DeviceId::new(n).unwrap()
}

fn header<'a>(request: &'a presage::proto::WebSocketRequestMessage, name: &str) -> Option<&'a str> {
    request.headers.iter().find_map(|h| {
        let (key, value) = h.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value)
    })
}

// --- Wire format ---------------------------------------------------------

/// A story goes with no authorization and `story=true`, as libsignal-net's
/// `send_multi_recipient_message` sends it.
#[test]
fn story_request_has_no_auth_and_the_story_flag() {
    let request = multi_recipient_request(vec![1, 2, 3], 1_700_000_000_123, &MultiRecipientAuth::Story, false, false);
    assert_eq!(request.verb.as_deref(), Some("PUT"));
    assert_eq!(
        request.path.as_deref(),
        Some("/v1/messages/multi_recipient?ts=1700000000123&online=false&urgent=false&story=true")
    );
    assert_eq!(header(&request, "content-type"), Some(MULTI_RECIPIENT_CONTENT_TYPE));
    assert_eq!(MULTI_RECIPIENT_CONTENT_TYPE, "application/vnd.signal-messenger.mrm");
    assert_eq!(header(&request, "group-send-token"), None);
    assert_eq!(header(&request, "unidentified-access-key"), None);
    assert_eq!(request.body.as_deref(), Some(&[1u8, 2, 3][..]));
}

/// A group message carries its token, base64 with padding, and no story
/// flag.
#[test]
fn group_request_carries_the_token() {
    let token = vec![0xfb, 0xff, 0x00, 0x10];
    let request =
        multi_recipient_request(vec![9], 42, &MultiRecipientAuth::GroupSendToken(token.clone()), true, true);
    assert_eq!(request.path.as_deref(), Some("/v1/messages/multi_recipient?ts=42&online=true&urgent=true"));
    use base64::Engine as _;
    assert_eq!(header(&request, "group-send-token"), Some(base64::engine::general_purpose::STANDARD.encode(&token).as_str()));
    assert_eq!(header(&request, "group-send-token"), Some("+/8AEA=="));
    assert_eq!(header(&request, "unidentified-access-key"), None);
}

/// The 409 and 410 bodies Desktop's schemas allow (either list left out)
/// read as the multi-recipient errors.
#[test]
fn mismatched_and_stale_bodies_parse() {
    let conflict: Vec<AccountMismatchedDevices> = serde_json::from_str(&format!(
        r#"[{{"uuid":"{}","devices":{{"missingDevices":[2,3]}}}},{{"uuid":"{}","devices":{{"extraDevices":[4]}}}}]"#,
        sid(1).service_id_string(),
        sid(2).service_id_string()
    ))
    .unwrap();
    assert_eq!(conflict[0].uuid, sid(1));
    assert_eq!(conflict[0].devices.missing_devices, vec![device(2), device(3)]);
    assert!(conflict[0].devices.extra_devices.is_empty());
    assert_eq!(conflict[1].devices.extra_devices, vec![device(4)]);
    let gone: Vec<AccountStaleDevices> =
        serde_json::from_str(&format!(r#"[{{"uuid":"{}","devices":{{"staleDevices":[1]}}}}]"#, sid(3).service_id_string()))
            .unwrap();
    assert_eq!(gone[0].devices.stale_devices, vec![device(1)]);
}

/// Someone with a session and a pre-key bundle, as a recipient device.
struct Peer {
    address: ProtocolAddress,
    store: InMemSignalProtocolStore,
}

async fn peer(n: u8, device_id: u8, registration_id: u32, identity: IdentityKeyPair) -> (Peer, PreKeyBundle) {
    let mut rng = rand::rng();
    let mut store = InMemSignalProtocolStore::new(identity, registration_id).unwrap();
    let pre_key = KeyPair::generate(&mut rng);
    let signed = KeyPair::generate(&mut rng);
    let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rng);
    let signed_sig = identity.private_key().calculate_signature(&signed.public_key.serialize(), &mut rng).unwrap();
    let kyber_sig = identity.private_key().calculate_signature(&kyber.public_key.serialize(), &mut rng).unwrap();
    let bundle = PreKeyBundle::new(
        registration_id,
        device(device_id),
        Some((7.into(), pre_key.public_key)),
        8.into(),
        signed.public_key,
        signed_sig.to_vec(),
        9.into(),
        kyber.public_key.clone(),
        kyber_sig.to_vec(),
        *identity.identity_key(),
    )
    .unwrap();
    store.save_pre_key(7.into(), &PreKeyRecord::new(7.into(), &pre_key)).await.unwrap();
    store
        .save_signed_pre_key(8.into(), &SignedPreKeyRecord::new(8.into(), Timestamp::from_epoch_millis(1), &signed, &signed_sig))
        .await
        .unwrap();
    store
        .save_kyber_pre_key(9.into(), &KyberPreKeyRecord::new(9.into(), Timestamp::from_epoch_millis(1), &kyber, &kyber_sig))
        .await
        .unwrap();
    let address = ProtocolAddress::new(sid(n).service_id_string(), device(device_id));
    (Peer { address, store }, bundle)
}

/// The payload we upload is libsignal's sealed sender v2 for every device,
/// and each recipient can open it with our distribution message: what a
/// receiving client does with an envelope the server fans out.
#[tokio::test]
async fn sender_key_payload_reaches_each_device() {
    let mut rng = rand::rng();
    let me = ProtocolAddress::new(sid(9).service_id_string(), device(1));
    let mut ours = InMemSignalProtocolStore::new(IdentityKeyPair::generate(&mut rng), 1234).unwrap();
    // Two people; the second with two devices.
    let mut peers = Vec::new();
    // One identity key per person, shared by their devices.
    let identities = [IdentityKeyPair::generate(&mut rng), IdentityKeyPair::generate(&mut rng)];
    for (n, device_id, registration_id) in [(1, 1, 101), (2, 1, 201), (2, 3, 203)] {
        let (peer, bundle) = peer(n, device_id, registration_id, identities[n as usize - 1]).await;
        process_prekey_bundle(
            &peer.address,
            &me,
            &mut ours.session_store,
            &mut ours.identity_store,
            &bundle,
            SystemTime::now(),
            &mut rng,
        )
        .await
        .unwrap();
        peers.push(peer);
    }

    let distribution = Uuid::new_v4();
    let skdm = create_sender_key_distribution_message(&me, distribution, &mut ours, &mut rng).await.unwrap();
    let plaintext = b"a story for everyone".to_vec();
    let message = group_encrypt(&mut ours, &me, distribution, &plaintext, &mut rng).await.unwrap();

    let trust_root = KeyPair::generate(&mut rng);
    let server_key = KeyPair::generate(&mut rng);
    let server_cert = ServerCertificate::new(1, server_key.public_key, &trust_root.private_key, &mut rng).unwrap();
    let our_key = *ours.get_identity_key_pair().await.unwrap().public_key();
    let certificate = SenderCertificate::new(
        sid(9).service_id_string(),
        None,
        our_key,
        device(1),
        Timestamp::from_epoch_millis(u64::MAX / 2),
        server_cert,
        &server_key.private_key,
        &mut rng,
    )
    .unwrap();
    let group_id = vec![5u8; 32];
    let usmc = UnidentifiedSenderMessageContent::new(
        CiphertextMessageType::SenderKey,
        certificate,
        message.serialized().to_vec(),
        ContentHint::Implicit,
        Some(group_id.clone()),
    )
    .unwrap();
    let devices = vec![
        SenderKeyDevice { service_id: sid(2), device_id: device(3), registration_id: 203 },
        SenderKeyDevice { service_id: sid(1), device_id: device(1), registration_id: 101 },
        SenderKeyDevice { service_id: sid(2), device_id: device(1), registration_id: 201 },
    ];
    let payload = multi_recipient_payload(&ours, &devices, &usmc).await.unwrap();

    let parsed = SealedSenderV2SentMessage::parse(&payload).unwrap();
    assert_eq!(parsed.recipients.len(), 2);
    let mut first = parsed.recipients[&sid(1)].devices.clone();
    first.sort();
    assert_eq!(first, vec![(device(1), 101)]);
    let mut second = parsed.recipients[&sid(2)].devices.clone();
    second.sort();
    assert_eq!(second, vec![(device(1), 201), (device(3), 203)]);

    for (n, peer) in [1u8, 2, 2].into_iter().zip(peers.iter_mut()) {
        let received = parsed.received_message_parts_for_recipient(&parsed.recipients[&sid(n)]).as_ref().concat();
        let opened = sealed_sender_decrypt_to_usmc(&received, &peer.store.identity_store).await.unwrap();
        assert_eq!(opened.msg_type().unwrap(), CiphertextMessageType::SenderKey);
        assert_eq!(opened.content_hint().unwrap(), ContentHint::Implicit);
        assert_eq!(opened.group_id().unwrap(), Some(&group_id[..]));
        let skdm = SenderKeyDistributionMessage::try_from(skdm.serialized()).unwrap();
        process_sender_key_distribution_message(&me, &skdm, &mut peer.store).await.unwrap();
        let decrypted = group_decrypt(opened.contents().unwrap(), &mut peer.store, &me).await.unwrap();
        assert_eq!(decrypted, plaintext);
    }
}

// --- Group send endorsements ----------------------------------------------

/// 12:00 on a day; the server's endorsements expire at the end of the
/// next one (36 hours on).
const DAY: u64 = 19_700 * SECONDS_PER_DAY;
const NOW: u64 = DAY + 12 * 60 * 60;
const EXPIRES: u64 = DAY + 2 * SECONDS_PER_DAY;

struct Issued {
    server: ServerSecretParams,
    group: GroupSecretParams,
    members: Vec<Aci>,
    endorsements: GroupSendEndorsements,
}

/// A group of `count` (us first), its endorsements issued by a made-up
/// server and received as the group server's response would be.
fn issue(count: u8) -> Issued {
    let mut rng = rand::rng();
    let server = ServerSecretParams::generate(rng.random());
    let group = GroupSecretParams::generate(rng.random());
    let members: Vec<Aci> = (1..=count).map(aci).collect();
    let key_pair = GroupSendDerivedKeyPair::for_expiration(zkgroup::Timestamp::from_epoch_seconds(EXPIRES), &server);
    let response = GroupSendEndorsementsResponse::issue(
        members.iter().map(|m| group.encrypt_service_id((*m).into())),
        &key_pair,
        rng.random(),
    );
    let endorsements = GroupSendEndorsements::receive(
        &zkgroup::serialize(&response),
        &members,
        members[0],
        &group,
        &server.get_public_params(),
        NOW,
    )
    .unwrap();
    Issued { server, group, members, endorsements }
}

/// The chat server's check of a `Group-Send-Token`.
fn server_accepts(issued: &Issued, token: &[u8], recipients: &[Aci]) -> bool {
    let token: GroupSendFullToken = zkgroup::deserialize(token).unwrap();
    let key_pair = GroupSendDerivedKeyPair::for_expiration(token.expiration(), &issued.server);
    token
        .verify(recipients.iter().map(|a| ServiceId::from(*a)), zkgroup::Timestamp::from_epoch_seconds(NOW), &key_pair)
        .is_ok()
}

#[test]
fn endorsements_are_kept_for_each_member() {
    let issued = issue(5);
    assert_eq!(issued.endorsements.expiration, EXPIRES);
    assert_eq!(issued.endorsements.members.len(), 5);
    for member in &issued.members {
        assert!(issued.endorsements.has_member(&(*member).into()));
    }
    assert!(!issued.endorsements.has_member(&sid(40)));
    // They keep (as JSON in a setting) and read back the same.
    let json = serde_json::to_string(&issued.endorsements).unwrap();
    assert_eq!(read_endorsements(Some(&json)), Some(issued.endorsements.clone()));
    assert_eq!(read_endorsements(Some("not json")), None);
}

/// Every way Desktop's `buildToken` makes the endorsement gives a token the
/// server takes for exactly those recipients.
#[test]
fn tokens_verify_for_exactly_their_recipients() {
    let issued = issue(9);
    let us = issued.members[0];
    let token_for = |recipients: &[Aci]| {
        let set: HashSet<ServiceId> = recipients.iter().map(|a| ServiceId::from(*a)).collect();
        issued.endorsements.build_token(&set, us, &issued.group, NOW).unwrap()
    };
    let others: Vec<Aci> = issued.members[1..].to_vec();
    // One member: their own endorsement.
    let one = token_for(&others[..1]);
    assert!(server_accepts(&issued, &one, &others[..1]));
    assert!(!server_accepts(&issued, &one, &others[..2]));
    // Everyone but us: the combined endorsement.
    let all = token_for(&others);
    assert!(server_accepts(&issued, &all, &others));
    assert!(!server_accepts(&issued, &all, &issued.members));
    // Most of them: the combined one less those left out.
    let most = token_for(&others[..6]);
    assert!(server_accepts(&issued, &most, &others[..6]));
    assert!(!server_accepts(&issued, &most, &others));
    // A few: theirs combined.
    let few = token_for(&others[..3]);
    assert!(server_accepts(&issued, &few, &others[..3]));
    // Including us: the combined one with ours added.
    let with_us = token_for(&issued.members);
    assert!(server_accepts(&issued, &with_us, &issued.members));
}

#[test]
fn tokens_need_an_endorsement_for_everyone() {
    let issued = issue(3);
    let set: HashSet<ServiceId> = [sid(2), sid(40)].into_iter().collect();
    assert!(issued.endorsements.build_token(&set, issued.members[0], &issued.group, NOW).is_err());
}

/// Desktop's `validateGroupSendEndorsementsExpiration`: more than two
/// hours and less than two days left.
#[test]
fn endorsements_expire_as_desktop_judges() {
    let issued = issue(3);
    let members: Vec<ServiceId> = issued.members.iter().map(|a| ServiceId::from(*a)).collect();
    assert!(endorsements_usable(&issued.endorsements, &members, NOW));
    assert!(endorsements_usable(&issued.endorsements, &members, EXPIRES - 2 * 60 * 60 - 1));
    assert!(!endorsements_usable(&issued.endorsements, &members, EXPIRES - 2 * 60 * 60));
    assert!(!endorsements_usable(&issued.endorsements, &members, EXPIRES + 1));
    assert!(!endorsements_usable(&issued.endorsements, &members, EXPIRES - 2 * SECONDS_PER_DAY));
    assert!(endorsements_usable(&issued.endorsements, &members, EXPIRES - 2 * SECONDS_PER_DAY + 1));
    // A member without one (someone joined since): fetch them again.
    let mut with_new = members.clone();
    with_new.push(sid(40));
    assert!(!endorsements_usable(&issued.endorsements, &with_new, NOW));
    // No token once they're too close to expiring.
    let set: HashSet<ServiceId> = [sid(2)].into_iter().collect();
    assert!(issued.endorsements.build_token(&set, issued.members[0], &issued.group, EXPIRES - 60).is_err());
}

#[test]
fn bad_endorsement_responses_are_refused() {
    let issued = issue(3);
    let params = issued.server.get_public_params();
    assert!(GroupSendEndorsements::receive(&[], &issued.members, issued.members[0], &issued.group, &params, NOW).is_err());
    assert!(GroupSendEndorsements::receive(&[1, 2, 3], &issued.members, issued.members[0], &issued.group, &params, NOW).is_err());
    // Endorsements for other members don't verify.
    let mut rng = rand::rng();
    let key_pair = GroupSendDerivedKeyPair::for_expiration(zkgroup::Timestamp::from_epoch_seconds(EXPIRES), &issued.server);
    let response = GroupSendEndorsementsResponse::issue(
        [aci(30), aci(31), aci(32)].iter().map(|m| issued.group.encrypt_service_id((*m).into())),
        &key_pair,
        rng.random(),
    );
    assert!(
        GroupSendEndorsements::receive(&zkgroup::serialize(&response), &issued.members, issued.members[0], &issued.group, &params, NOW)
            .is_err()
    );
}

// --- The send loop, against a fake network -------------------------------

#[derive(Default)]
struct Fake {
    /// Open sessions: person -> (device, registration id).
    sessions: HashMap<ServiceId, Vec<(u8, u32)>>,
    /// What a pre-key fetch finds; missing: not registered.
    prekeys: HashMap<ServiceId, Vec<(u8, u32)>>,
    /// Distribution messages to these fail.
    skdm_fails: HashSet<ServiceId>,
    /// Our sender keys in the store.
    keys: HashSet<Uuid>,
    /// Answers to each sender key send, in turn (then: sent).
    answers: VecDeque<Result<Vec<ServiceId>, MessageSenderError>>,
    fetched: Vec<(ServiceId, Option<Vec<u8>>)>,
    archived: Vec<(ServiceId, Vec<u8>)>,
    skdms: Vec<(Vec<ServiceId>, Uuid, bool)>,
    sends: Vec<(Vec<SenderKeyDevice>, Uuid, MultiRecipientAuth)>,
}

impl Fake {
    fn with_sessions(people: &[(u8, &[(u8, u32)])]) -> Self {
        let mut fake = Fake::default();
        for (n, devices) in people {
            fake.sessions.insert(sid(*n), devices.to_vec());
            fake.prekeys.insert(sid(*n), devices.to_vec());
        }
        fake
    }
}

impl SenderKeyTransport for Fake {
    async fn open_devices(
        &mut self,
        recipients: &[ServiceId],
    ) -> Result<(Vec<SenderKeyDevice>, Vec<ServiceId>), MessageSenderError> {
        let mut devices = Vec::new();
        let mut empty = Vec::new();
        for r in recipients {
            match self.sessions.get(r).filter(|d| !d.is_empty()) {
                Some(list) => devices.extend(list.iter().map(|(d, reg)| SenderKeyDevice {
                    service_id: *r,
                    device_id: device(*d),
                    registration_id: *reg,
                })),
                None => empty.push(*r),
            }
        }
        Ok((devices, empty))
    }

    async fn start_sessions(&mut self, recipient: &ServiceId, devices: Option<&[DeviceId]>) -> Result<(), MessageSenderError> {
        self.fetched.push((*recipient, devices.map(|d| d.iter().map(|d| u8::from(*d)).collect())));
        let Some(found) = self.prekeys.get(recipient).cloned() else {
            return Err(MessageSenderError::NotFound { service_id: *recipient });
        };
        let open = self.sessions.entry(*recipient).or_default();
        for (d, reg) in found {
            if devices.is_none_or(|want| want.contains(&device(d))) {
                open.retain(|(od, _)| *od != d);
                open.push((d, reg));
            }
        }
        Ok(())
    }

    async fn archive_sessions(&mut self, recipient: &ServiceId, devices: &[DeviceId]) -> Result<(), MessageSenderError> {
        let devices: Vec<u8> = devices.iter().map(|d| u8::from(*d)).collect();
        if let Some(open) = self.sessions.get_mut(recipient) {
            open.retain(|(d, _)| !devices.contains(d));
        }
        self.archived.push((*recipient, devices));
        Ok(())
    }

    async fn has_sender_key(&mut self, distribution_id: Uuid) -> Result<bool, MessageSenderError> {
        Ok(self.keys.contains(&distribution_id))
    }

    async fn send_distribution_message(
        &mut self,
        recipients: &[ServiceId],
        distribution_id: Uuid,
        story: bool,
    ) -> Vec<(ServiceId, Result<(), MessageSenderError>)> {
        self.keys.insert(distribution_id);
        self.skdms.push((recipients.to_vec(), distribution_id, story));
        recipients
            .iter()
            .map(|r| {
                let result = if self.skdm_fails.contains(r) {
                    Err(MessageSenderError::ServiceError(ServiceError::Unauthorized))
                } else {
                    Ok(())
                };
                (*r, result)
            })
            .collect()
    }

    async fn send_sender_key_message(
        &mut self,
        devices: &[SenderKeyDevice],
        distribution_id: Uuid,
        auth: &MultiRecipientAuth,
    ) -> Result<Vec<ServiceId>, MessageSenderError> {
        self.sends.push((devices.to_vec(), distribution_id, auth.clone()));
        self.answers.pop_front().unwrap_or(Ok(Vec::new()))
    }
}

const T: u64 = 1_700_000_000_000;

fn story(recipients: &[u8]) -> SenderKeyRequest<'static> {
    SenderKeyRequest { recipients: recipients.iter().map(|n| sid(*n)).collect(), story: true, group: false, auth: None, now_ms: T, partial: false }
}

#[allow(clippy::result_large_err)]
fn run<Tr: SenderKeyTransport>(
    fake: &mut Tr,
    info: &mut SenderKeyInfo,
    request: &SenderKeyRequest<'_>,
) -> Result<SenderKeyOutcome, MessageSenderError> {
    futures::executor::block_on(send_via_sender_key(fake, info, request))
}

fn ids(list: &[u8]) -> Vec<ServiceId> {
    list.iter().map(|n| sid(*n)).collect()
}

/// The first send hands everyone our key, then sends once; the next sends
/// only the message.
#[test]
fn first_send_distributes_our_key_then_sends_once() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21), (2, 22)]), (3, &[(1, 31)])]);
    let mut info = SenderKeyInfo::new(T);
    let outcome = run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    assert_eq!(outcome, SenderKeyOutcome { sent: ids(&[1, 2, 3]), normal: vec![], unregistered: vec![] });
    assert_eq!(fake.skdms, vec![(ids(&[1, 2, 3]), info.distribution_id, true)]);
    assert_eq!(fake.sends.len(), 1);
    assert_eq!(fake.sends[0].0.len(), 4);
    assert_eq!(fake.sends[0].2, MultiRecipientAuth::Story);
    assert_eq!(info.member_devices.len(), 4);

    let outcome = run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 2, 3]));
    assert_eq!(fake.skdms.len(), 1, "no second distribution message");
    assert_eq!(fake.sends.len(), 2);
}

/// Who we have no session with gets one first; who isn't registered is
/// reported, and doesn't hold up the rest.
#[test]
fn sessions_are_started_and_the_unregistered_reported() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    fake.prekeys.insert(sid(3), vec![(1, 31)]);
    let mut info = SenderKeyInfo::new(T);
    let outcome = run(&mut fake, &mut info, &story(&[1, 2, 3, 4])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 2, 3]));
    assert_eq!(outcome.unregistered, ids(&[4]));
    assert!(outcome.normal.is_empty());
    assert_eq!(fake.fetched, vec![(sid(3), None), (sid(4), None)]);
}

/// Someone whose pre-keys don't give a session is left to the one by one
/// send (which says why), not fetched again and again.
#[test]
fn no_session_after_fetching_goes_one_by_one() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    fake.prekeys.insert(sid(3), Vec::new());
    let mut info = SenderKeyInfo::new(T);
    let outcome = run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 2]));
    assert_eq!(outcome.normal, ids(&[3]));
    assert_eq!(fake.fetched.len(), 1);
}

/// When our key doesn't reach someone, they aren't recorded as having it
/// and get the message one by one.
#[test]
fn failed_distribution_goes_one_by_one_and_is_not_recorded() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)]), (3, &[(1, 31)])]);
    fake.skdm_fails.insert(sid(2));
    let mut info = SenderKeyInfo::new(T);
    let outcome = run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 3]));
    assert_eq!(outcome.normal, ids(&[2]));
    assert!(info.member_devices.iter().all(|d| d.service_id != sid(2)));
    assert!(fake.sends[0].0.iter().all(|d| d.service_id != sid(2)));
}

/// Desktop: fewer than two to send to isn't worth a sender key.
#[test]
fn fewer_than_two_falls_over() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)])]);
    let mut info = SenderKeyInfo::new(T);
    let error = run(&mut fake, &mut info, &story(&[1])).unwrap_err();
    assert!(matches!(error, MessageSenderError::NotEnoughRecipients));
    assert!(error.sender_key_may_fall_over());
    assert!(fake.skdms.is_empty());
}

/// A group message without endorsements goes one by one, before any work
/// (Desktop once `retireAccessKeyGroupSend` is on).
#[test]
fn group_message_without_endorsements_falls_over_at_once() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    let request = SenderKeyRequest { recipients: ids(&[1, 2]), story: false, group: true, auth: None, now_ms: T, partial: false };
    let error = run(&mut fake, &mut info, &request).unwrap_err();
    assert!(matches!(error, MessageSenderError::MissingGroupSendToken));
    assert!(error.sender_key_may_fall_over());
    assert!(fake.skdms.is_empty() && fake.sends.is_empty() && fake.fetched.is_empty());
}

/// A group message goes with a token for exactly those it reaches by
/// sender key; members without an endorsement go one by one; distribution
/// messages for a group aren't stories.
#[test]
fn group_message_sends_with_a_token() {
    let issued = issue(4);
    // Members 2 to 4 (1 is us); 5 has no endorsement.
    let mut fake = Fake::with_sessions(&[(2, &[(1, 21)]), (3, &[(1, 31)]), (4, &[(1, 41)]), (5, &[(1, 51)])]);
    let mut info = SenderKeyInfo::new(NOW * 1000);
    let request = SenderKeyRequest {
        recipients: ids(&[2, 3, 4, 5]),
        story: false,
        group: true,
        auth: Some(GroupSendAuth { endorsements: &issued.endorsements, group_secret_params: issued.group, our_aci: aci(1) }),
        now_ms: NOW * 1000,
        partial: false,
    };
    let outcome = run(&mut fake, &mut info, &request).unwrap();
    assert_eq!(outcome.sent, ids(&[2, 3, 4]));
    assert_eq!(outcome.normal, ids(&[5]));
    assert!(!fake.skdms[0].2, "a group's distribution message isn't a story");
    let MultiRecipientAuth::GroupSendToken(token) = &fake.sends[0].2 else { panic!("no token") };
    assert!(server_accepts(&issued, token, &[aci(2), aci(3), aci(4)]));
    assert!(!server_accepts(&issued, token, &[aci(2), aci(3), aci(4), aci(5)]));
}

/// A group story needs no token, and its distribution messages go as
/// ordinary group messages.
#[test]
fn group_story_needs_no_token() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    let request = SenderKeyRequest { recipients: ids(&[1, 2]), story: true, group: true, auth: None, now_ms: T, partial: false };
    run(&mut fake, &mut info, &request).unwrap();
    assert!(!fake.skdms[0].2);
    assert_eq!(fake.sends[0].2, MultiRecipientAuth::Story);
}

/// Someone who had our key and isn't a recipient any more: a new key for
/// everyone (Desktop's `resetSenderKey`).
#[test]
fn removed_recipient_resets_the_key() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)]), (3, &[(1, 31)])]);
    let mut info = SenderKeyInfo::new(T);
    run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    let old = info.distribution_id;
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_ne!(info.distribution_id, old);
    assert_eq!(fake.skdms.last().unwrap(), &(ids(&[1, 2]), info.distribution_id, true));
    assert_eq!(info.member_devices.len(), 2);
}

/// A device gone from someone still a recipient is just forgotten.
#[test]
fn removed_device_of_a_recipient_keeps_the_key() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11), (2, 12)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    let old = info.distribution_id;
    fake.sessions.insert(sid(1), vec![(1, 11)]);
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_eq!(info.distribution_id, old);
    assert_eq!(fake.skdms.len(), 1);
    assert_eq!(info.member_devices.len(), 2);
}

/// A re-registered device (new registration id) gets our key again.
#[test]
fn reregistered_device_gets_the_key_again() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    fake.sessions.insert(sid(2), vec![(1, 99)]);
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_eq!(fake.skdms.last().unwrap().0, ids(&[2]));
    assert!(info.member_devices.iter().any(|d| d.registration_id == 99));
    assert!(info.member_devices.iter().all(|d| d.registration_id != 21));
}

/// After 90 days, a new key.
#[test]
fn old_key_is_replaced() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T - SENDER_KEY_MAX_AGE_MS - 1);
    let old = info.distribution_id;
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_ne!(info.distribution_id, old);
    assert_eq!(info.created_at, T);
}

/// Our key gone from the store while others are recorded as having it: a
/// new one.
#[test]
fn lost_key_is_replaced() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    let old = info.distribution_id;
    fake.keys.clear();
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_ne!(info.distribution_id, old);
    assert_eq!(fake.skdms.len(), 2);
}

/// Desktop's `handle409Response`: sessions for missing devices, extra ones
/// archived, and the send tried again.
#[test]
fn mismatched_devices_are_fixed_and_retried() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11), (5, 15)]), (2, &[(1, 21)])]);
    fake.prekeys.insert(sid(2), vec![(1, 21), (2, 22)]);
    let mut info = SenderKeyInfo::new(T);
    fake.answers.push_back(Err(MessageSenderError::ServiceError(ServiceError::MultiRecipientMismatchedDevices(vec![
        AccountMismatchedDevices {
            uuid: sid(1),
            devices: MismatchedDevices { missing_devices: vec![], extra_devices: vec![device(5)] },
        },
        AccountMismatchedDevices {
            uuid: sid(2),
            devices: MismatchedDevices { missing_devices: vec![device(2)], extra_devices: vec![] },
        },
    ]))));
    let outcome = run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 2]));
    assert_eq!(fake.archived, vec![(sid(1), vec![5])]);
    assert_eq!(fake.fetched, vec![(sid(2), Some(vec![2]))]);
    // The new device got our key before the retry; the extra one is gone.
    assert_eq!(fake.skdms.last().unwrap().0, ids(&[2]));
    let last: Vec<(ServiceId, u8)> = fake.sends.last().unwrap().0.iter().map(|d| (d.service_id, u8::from(d.device_id))).collect();
    assert!(last.contains(&(sid(2), 2)) && !last.contains(&(sid(1), 5)));
}

/// Desktop's `handle410Response`: new sessions with stale devices, which
/// get our key again.
#[test]
fn stale_devices_get_new_sessions_and_our_key() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    fake.prekeys.insert(sid(2), vec![(1, 77)]);
    fake.answers.push_back(Err(MessageSenderError::ServiceError(ServiceError::MultiRecipientStaleDevices(vec![
        AccountStaleDevices { uuid: sid(2), devices: StaleDevices { stale_devices: vec![device(1)] } },
    ]))));
    run(&mut fake, &mut info, &story(&[1, 2])).unwrap();
    assert_eq!(fake.archived, vec![(sid(2), vec![1])]);
    assert_eq!(fake.skdms.last().unwrap().0, ids(&[2]));
    assert!(info.member_devices.iter().any(|d| d.service_id == sid(2) && d.registration_id == 77));
}

/// The server naming someone unregistered in its answer.
#[test]
fn unregistered_in_the_answer_are_reported() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)]), (3, &[(1, 31)])]);
    fake.answers.push_back(Ok(vec![sid(3)]));
    let mut info = SenderKeyInfo::new(T);
    let outcome = run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 2]));
    assert_eq!(outcome.unregistered, ids(&[3]));
}

/// A server that keeps asking for changes doesn't keep us looping.
#[test]
fn endless_conflicts_stop() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    for _ in 0..=MAX_ATTEMPTS + 1 {
        fake.answers.push_back(Err(MessageSenderError::ServiceError(ServiceError::MultiRecipientMismatchedDevices(
            vec![],
        ))));
    }
    let mut info = SenderKeyInfo::new(T);
    let error = run(&mut fake, &mut info, &story(&[1, 2])).unwrap_err();
    assert!(matches!(error, MessageSenderError::SenderKeyAttempts));
    assert!(error.sender_key_may_fall_over());
    assert!(fake.sends.len() <= MAX_ATTEMPTS);
}

/// Desktop's `_shouldFailSend`: what fails the send outright, and what
/// falls over to one by one.
#[test]
fn which_failures_fall_over() {
    let falls_over = |e: MessageSenderError| e.sender_key_may_fall_over();
    // The multi-recipient endpoint's 401 and 404.
    assert!(falls_over(MessageSenderError::ServiceError(ServiceError::Unauthorized)));
    assert!(falls_over(MessageSenderError::ServiceError(ServiceError::NotFoundError)));
    assert!(falls_over(MessageSenderError::NotEnoughRecipients));
    assert!(falls_over(MessageSenderError::Endorsement("expired".into())));
    // Rate limits, challenges, the network, trust.
    assert!(!falls_over(MessageSenderError::ServiceError(ServiceError::RateLimitExceeded { retry_after: None })));
    assert!(!falls_over(MessageSenderError::ProofRequired { token: "t".into(), options: vec![] }));
    assert!(!falls_over(MessageSenderError::ServiceError(ServiceError::ServerRejected)));
    assert!(!falls_over(MessageSenderError::ServiceError(ServiceError::WsClosing { reason: "gone" })));
    assert!(!falls_over(MessageSenderError::UntrustedIdentity { address: sid(1) }));
}

/// An error before the send (a session that can't be started) is returned
/// with the key state kept as far as it got.
#[test]
fn errors_keep_the_state_so_far() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)])]);
    let mut info = SenderKeyInfo::new(T);
    fake.answers.push_back(Err(MessageSenderError::ServiceError(ServiceError::RateLimitExceeded { retry_after: None })));
    let error = run(&mut fake, &mut info, &story(&[1, 2])).unwrap_err();
    assert!(!error.sender_key_may_fall_over());
    assert_eq!(info.member_devices.len(), 2, "our key reached them; that's kept");
}

#[test]
fn device_changes_are_found() {
    let d = |n: u8, dev: u8, reg: u32| SenderKeyDevice { service_id: sid(n), device_id: device(dev), registration_id: reg };
    let changes = analyze_sender_key_devices(&[d(1, 1, 11), d(2, 1, 21), d(2, 2, 22)], &[d(1, 1, 11), d(2, 1, 99), d(3, 1, 31)], false);
    assert_eq!(changes.new_devices, vec![d(2, 1, 99), d(3, 1, 31)]);
    assert_eq!(changes.new_service_ids, ids(&[2, 3]));
    assert_eq!(changes.removed_devices, vec![d(2, 1, 21), d(2, 2, 22)]);
    assert_eq!(changes.removed_service_ids, ids(&[2]));
    // A partial send (Desktop's `isPartialSend`) reports no removals.
    let partial = analyze_sender_key_devices(&[d(1, 1, 11), d(2, 1, 21)], &[d(1, 1, 11), d(3, 1, 31)], true);
    assert_eq!(partial.new_service_ids, ids(&[3]));
    assert!(partial.removed_devices.is_empty());
    assert!(partial.removed_service_ids.is_empty());
}

/// A retry to only some members (a partial send) keeps our key for the
/// rest, and gives it to a recipient who doesn't have it yet.
#[test]
fn partial_send_keeps_the_key_for_those_left_out() {
    let mut fake = Fake::with_sessions(&[(1, &[(1, 11)]), (2, &[(1, 21)]), (3, &[(1, 31)]), (4, &[(1, 41)])]);
    let mut info = SenderKeyInfo::new(T);
    run(&mut fake, &mut info, &story(&[1, 2, 3])).unwrap();
    let old = info.distribution_id;
    let partial = |r: &[u8]| SenderKeyRequest { partial: true, ..story(r) };
    let outcome = run(&mut fake, &mut info, &partial(&[1, 2])).unwrap();
    assert_eq!(outcome.sent, ids(&[1, 2]));
    assert_eq!(info.distribution_id, old);
    assert_eq!(fake.skdms.len(), 1, "everyone sent to has our key");
    assert_eq!(info.member_devices.len(), 3, "3 keeps our key");
    run(&mut fake, &mut info, &partial(&[2, 4])).unwrap();
    assert_eq!(info.distribution_id, old);
    assert_eq!(fake.skdms.last().unwrap(), &(ids(&[4]), old, true));
    assert_eq!(info.member_devices.len(), 4);
    // The same subset as a whole send: 1 and 3 left, so a new key.
    run(&mut fake, &mut info, &story(&[2, 4])).unwrap();
    assert_ne!(info.distribution_id, old);
}

// --- Keeping state, and story lists ---------------------------------------

#[test]
fn sender_key_state_keeps_and_reads_back() {
    let mut info = SenderKeyInfo::new(T);
    info.member_devices.push(SenderKeyDevice { service_id: sid(1), device_id: device(2), registration_id: 7 });
    let json = serde_json::to_string(&info).unwrap();
    assert_eq!(read_info(Some(&json), T + 5), info);
    // Missing or unreadable: a new one.
    let fresh = read_info(None, T + 5);
    assert_eq!(fresh.created_at, T + 5);
    assert!(fresh.member_devices.is_empty());
    assert_eq!(read_info(Some("{"), T + 5).created_at, T + 5);
    assert_eq!(sender_key_setting(&list_target("abc")), "sender_key.list:abc");
    assert_eq!(endorsements_setting("g:00ff"), "group_send_endorsements.g:00ff");
}

fn list(id: &str, allows_replies: bool) -> StoryList {
    StoryList { id: id.into(), name: id.into(), members: Vec::new(), block_list: false, allows_replies }
}

/// Desktop's `sendStoryMessage`: lists allowing replies first, each person
/// once, through the first list they're in.
#[test]
fn each_person_gets_a_story_through_one_list() {
    let people = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let lists = vec![
        (list("quiet", false), people(&["ana", "bo"])),
        (list("friends", true), people(&["bo", "cy"])),
        (list("family", true), people(&["cy", "di"])),
        (list("empty", false), people(&["ana"])),
    ];
    let sends = list_sends(&lists);
    assert_eq!(
        sends,
        vec![
            ListSend { list_id: "friends".into(), allows_replies: true, recipients: people(&["bo", "cy"]) },
            ListSend { list_id: "family".into(), allows_replies: true, recipients: people(&["di"]) },
            ListSend { list_id: "quiet".into(), allows_replies: false, recipients: people(&["ana"]) },
        ]
    );
}

#[test]
fn story_batch_plans_say_what_each_recipient_gets() {
    let outcome = SenderKeyOutcome { sent: ids(&[1]), normal: ids(&[2]), unregistered: ids(&[3]) };
    let plan = StoryBatchPlan::from_outcome(&outcome);
    assert_eq!(plan.for_recipient(&sid(1)), StoryBatchStep::Done(Ok(())));
    assert_eq!(plan.for_recipient(&sid(2)), StoryBatchStep::OneByOne);
    assert_eq!(plan.for_recipient(&sid(3)), StoryBatchStep::Done(Err("no longer on Signal".into())));
    // Anyone the outcome doesn't name goes one by one rather than nowhere.
    assert_eq!(plan.for_recipient(&sid(4)), StoryBatchStep::OneByOne);
    assert_eq!(StoryBatchPlan::all_one_by_one().for_recipient(&sid(1)), StoryBatchStep::OneByOne);
    assert_eq!(
        StoryBatchPlan::failed("rate limited".into()).for_recipient(&sid(1)),
        StoryBatchStep::Done(Err("rate limited".into()))
    );
}
