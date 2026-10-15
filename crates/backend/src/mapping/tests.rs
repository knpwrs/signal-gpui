use chrono::{TimeZone, Utc};
use presage::libsignal_service::{
    content::{Content, ContentBody, Metadata},
    protocol::{DeviceId, ServiceId},
};
use presage::proto::{
    AttachmentPointer, BodyRange, DataMessage, EditMessage, GroupContextV2, ReceiptMessage,
    SyncMessage, TypingMessage, body_range, data_message, receipt_message, sync_message,
    typing_message,
};
use signal_store::{ConversationId, DeliveryStatus, Direction};

use super::*;

const ME: &str = "11111111-1111-4111-8111-111111111111";
const ALICE: &str = "22222222-2222-4222-8222-222222222222";
const BOB: &str = "33333333-3333-4333-8333-333333333333";

fn ctx() -> MapContext<'static> {
    MapContext { self_aci: ME, now: 999 }
}

fn content(sender: &str, body: impl Into<ContentBody>) -> Content {
    let sender = ServiceId::parse_from_service_id_string(sender).unwrap();
    let ts = Utc.timestamp_millis_opt(500).unwrap();
    Content {
        metadata: Metadata {
            sender,
            destination: ServiceId::parse_from_service_id_string(ME).unwrap(),
            sender_device: DeviceId::try_from(1u8).unwrap(),
            pni_verified: None,
            client_timestamp: ts,
            server_timestamp: ts,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        },
        body: body.into(),
    }
}

fn text(body: &str, ts: u64) -> DataMessage {
    DataMessage { body: Some(body.into()), timestamp: Some(ts), ..Default::default() }
}

fn group_key() -> Vec<u8> {
    vec![7u8; 32]
}

#[test]
fn incoming_direct_text() {
    let actions = map_content(&content(ALICE, text("hello", 100)), &ctx());
    let [Action::Insert { message, mentions, pointers, .. }] = actions.as_slice() else {
        panic!("{actions:?}")
    };
    assert_eq!(message.conversation_id, ConversationId::direct(ALICE));
    assert_eq!(message.direction, Direction::Incoming);
    assert_eq!(message.author.as_deref(), Some(ALICE));
    assert_eq!((message.sent_at, message.received_at), (100, 999));
    assert_eq!(message.body, "hello");
    assert!(mentions.is_empty() && pointers.is_empty());
}

#[test]
fn timestamp_falls_back_to_envelope() {
    let dm = DataMessage { body: Some("x".into()), ..Default::default() };
    let actions = map_content(&content(ALICE, dm), &ctx());
    let [Action::Insert { message, .. }] = actions.as_slice() else { panic!() };
    assert_eq!(message.sent_at, 500);
}

#[test]
fn incoming_group_with_mentions_quote_and_attachment() {
    let dm = DataMessage {
        body: Some("hi \u{FFFC} and \u{FFFC}".into()),
        timestamp: Some(100),
        group_v2: Some(GroupContextV2 { master_key: Some(group_key()), revision: Some(3), ..Default::default() }),
        body_ranges: vec![
            BodyRange {
                start: Some(9),
                length: Some(1),
                associated_value: Some(body_range::AssociatedValue::MentionAci(BOB.into())),
            },
            BodyRange {
                start: Some(3),
                length: Some(1),
                associated_value: Some(body_range::AssociatedValue::MentionAci(ME.into())),
            },
            BodyRange {
                start: Some(0),
                length: Some(2),
                associated_value: Some(body_range::AssociatedValue::Style(1)),
            },
        ],
        quote: Some(data_message::Quote {
            id: Some(42),
            author_aci: Some(ME.into()),
            text: Some("original".into()),
            ..Default::default()
        }),
        attachments: vec![AttachmentPointer {
            content_type: Some("image/png".into()),
            size: Some(1234),
            width: Some(10),
            height: Some(0),
            file_name: Some("cat.png".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let actions = map_content(&content(ALICE, dm), &ctx());
    let [Action::Insert { message, mentions, pointers, .. }] = actions.as_slice() else { panic!() };
    assert_eq!(message.conversation_id, ConversationId::group(&group_key()));
    assert_eq!(mentions, &[ME.to_owned(), BOB.to_owned()]);
    let quote = message.quote.as_ref().unwrap();
    assert_eq!((quote.author.as_deref(), quote.sent_at, quote.text.as_str()), (None, 42, "original"));
    assert_eq!(pointers.len(), 1);
    let att = &message.attachments[0];
    assert_eq!((att.content_type.as_str(), att.size, att.width, att.height), ("image/png", 1234, Some(10), None));
    assert_eq!(render_mentions(&message.body, &["You".into(), "Bob".into()]), "hi @You and @Bob");
}

#[test]
fn render_mentions_with_missing_names() {
    assert_eq!(render_mentions("\u{FFFC} \u{FFFC}", &["A".into()]), "@A @Unknown");
    assert_eq!(render_mentions("plain", &[]), "plain");
}

#[test]
fn bad_group_key_is_dropped() {
    let dm = DataMessage {
        body: Some("x".into()),
        group_v2: Some(GroupContextV2 { master_key: Some(vec![1, 2, 3]), ..Default::default() }),
        ..Default::default()
    };
    assert!(map_content(&content(ALICE, dm), &ctx()).is_empty());
}

#[test]
fn empty_messages_are_skipped() {
    let dm = DataMessage { profile_key: Some(vec![1; 32]), timestamp: Some(1), ..Default::default() };
    assert!(map_content(&content(ALICE, dm), &ctx()).is_empty());
}

#[test]
fn sticker_uses_emoji_body() {
    let dm = DataMessage {
        timestamp: Some(1),
        sticker: Some(data_message::Sticker {
            emoji: Some("🐱".into()),
            data: Some(AttachmentPointer { content_type: Some("image/webp".into()), ..Default::default() }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let actions = map_content(&content(ALICE, dm), &ctx());
    let [Action::Insert { message, pointers, .. }] = actions.as_slice() else { panic!() };
    assert_eq!(message.body, "🐱");
    assert_eq!(pointers.len(), 1);
}

#[test]
fn reaction_targets_self_as_none() {
    let dm = DataMessage {
        timestamp: Some(200),
        reaction: Some(data_message::Reaction {
            emoji: Some("👍".into()),
            remove: Some(false),
            target_author_aci: Some(ME.into()),
            target_sent_timestamp: Some(100),
            ..Default::default()
        }),
        ..Default::default()
    };
    let actions = map_content(&content(ALICE, dm), &ctx());
    assert_eq!(
        actions,
        vec![Action::React {
            conversation: ConversationId::direct(ALICE),
            target_author: None,
            target_sent_at: 100,
            emoji: "👍".into(),
            author: Some(ALICE.into()),
            sent_at: 200,
            remove: false,
        }]
    );
}

#[test]
fn delete_and_expire_timer() {
    let dm = DataMessage {
        timestamp: Some(200),
        delete: Some(data_message::Delete { target_sent_timestamp: Some(100) }),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ALICE, dm), &ctx()),
        vec![Action::Delete {
            conversation: ConversationId::direct(ALICE),
            target_author: Some(ALICE.into()),
            target_sent_at: 100,
            sent_at: 200,
        }]
    );

    let dm = DataMessage {
        timestamp: Some(200),
        flags: Some(data_message::Flags::ExpirationTimerUpdate as u32),
        expire_timer: Some(3600),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ALICE, dm), &ctx()),
        vec![Action::ExpireTimer {
            conversation: ConversationId::direct(ALICE),
            seconds: 3600,
            version: None,
            by: Some(ALICE.into()),
            at: 200,
        }]
    );
}

#[test]
fn incoming_edit() {
    let edit = EditMessage { target_sent_timestamp: Some(100), data_message: Some(text("fixed", 300)) };
    assert_eq!(
        map_content(&content(ALICE, edit), &ctx()),
        vec![Action::Edit {
            conversation: ConversationId::direct(ALICE),
            target_author: Some(ALICE.into()),
            target_sent_at: 100,
            sent_at: 300,
            body: "fixed".into(),
            mentions: vec![],
        }]
    );
}

#[test]
fn sync_sent_transcript_is_outgoing() {
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            destination_service_id: Some(ALICE.into()),
            timestamp: Some(700),
            message: Some(text("from my phone", 700)),
            ..Default::default()
        })),
        read: vec![sync_message::Read {
            sender_aci: Some(BOB.into()),
            timestamp: Some(50),
            ..Default::default()
        }],
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    let [Action::Insert { message, .. }, Action::ReadSync(reads)] = actions.as_slice() else {
        panic!("{actions:?}")
    };
    assert_eq!(message.conversation_id, ConversationId::direct(ALICE));
    assert_eq!(message.direction, Direction::Outgoing);
    assert_eq!(message.author, None);
    assert_eq!(message.status, DeliveryStatus::Sent);
    assert_eq!(message.sent_at, 700);
    assert_eq!(reads, &[(BOB.to_owned(), 50)]);
}

#[test]
fn sync_sent_edit_to_group() {
    let mut dm = text("edited", 800);
    dm.group_v2 = Some(GroupContextV2 { master_key: Some(group_key()), ..Default::default() });
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            timestamp: Some(800),
            edit_message: Some(EditMessage { target_sent_timestamp: Some(700), data_message: Some(dm) }),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ME, sync), &ctx()),
        vec![Action::Edit {
            conversation: ConversationId::group(&group_key()),
            target_author: None,
            target_sent_at: 700,
            sent_at: 800,
            body: "edited".into(),
            mentions: vec![],
        }]
    );
}

#[test]
fn receipts_and_typing() {
    let receipt = ReceiptMessage {
        r#type: Some(receipt_message::Type::Read.into()),
        timestamp: vec![1, 2],
    };
    assert_eq!(
        map_content(&content(ALICE, receipt), &ctx()),
        vec![Action::Receipt { sent_at: vec![1, 2], status: DeliveryStatus::Read }]
    );
    let receipt = ReceiptMessage { r#type: None, timestamp: vec![3] };
    assert_eq!(
        map_content(&content(ALICE, receipt), &ctx()),
        vec![Action::Receipt { sent_at: vec![3], status: DeliveryStatus::Delivered }]
    );
    let empty = ReceiptMessage { r#type: None, timestamp: vec![] };
    assert!(map_content(&content(ALICE, empty), &ctx()).is_empty());

    let typing = TypingMessage {
        timestamp: Some(1),
        action: Some(typing_message::Action::Stopped.into()),
        group_id: Some(vec![9; 32]),
    };
    assert_eq!(
        map_content(&content(ALICE, typing), &ctx()),
        vec![Action::Typing { sender: ALICE.into(), group_id: Some(vec![9; 32]), started: false }]
    );
}

#[test]
fn attachment_extension() {
    use crate::actor::extension_for;
    let p = |name: Option<&str>, ct: &str| AttachmentPointer {
        file_name: name.map(Into::into),
        content_type: Some(ct.into()),
        ..Default::default()
    };
    assert_eq!(extension_for(&p(Some("Photo.JPEG"), "image/jpeg")), "jpeg");
    assert_eq!(extension_for(&p(None, "image/jpeg")), "jpg");
    assert_eq!(extension_for(&p(Some("../../etc/passwd"), "text/plain")), "txt");
    assert_eq!(extension_for(&p(Some("noext"), "application/x-unknown")), "bin");
}

#[test]
fn storage_fetch_latest_requests_a_resync() {
    let fetch = |kind: sync_message::fetch_latest::Type| SyncMessage {
        content: Some(sync_message::Content::FetchLatest(sync_message::FetchLatest { r#type: Some(kind as i32) })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, fetch(sync_message::fetch_latest::Type::StorageManifest)), &ctx());
    assert!(matches!(actions.as_slice(), [Action::StorageChanged]), "{actions:?}");
    let actions = map_content(&content(ME, fetch(sync_message::fetch_latest::Type::LocalProfile)), &ctx());
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn link_previews_only_for_links_in_the_message() {
    let preview = |url: &str| presage::proto::Preview {
        url: Some(url.into()),
        title: Some("Signal".into()),
        description: Some("".into()),
        date: Some(1_700_000_000_000),
        image: Some(AttachmentPointer { content_type: Some("image/png".into()), width: Some(1200), height: Some(630), ..Default::default() }),
    };
    let mut dm = text("look: https://signal.org/blog/", 100);
    dm.preview = vec![preview("https://signal.org/blog/")];
    let actions = map_content(&content(ALICE, dm.clone()), &ctx());
    let [Action::Insert { message, preview_image, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    let shown = message.link_preview.as_ref().expect("valid preview kept");
    assert_eq!((shown.url.as_str(), shown.title.as_deref()), ("https://signal.org/blog/", Some("Signal")));
    // Empty strings are no description.
    assert_eq!(shown.description, None);
    assert_eq!(shown.image.as_ref().and_then(|i| i.width), Some(1200));
    assert!(preview_image.is_some());

    // A preview for a link the message doesn't contain is dropped.
    dm.preview = vec![preview("https://evil.org/")];
    let actions = map_content(&content(ALICE, dm), &ctx());
    let [Action::Insert { message, preview_image, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert!(message.link_preview.is_none() && preview_image.is_none());
}

#[test]
fn delete_for_me_sync_names_messages_and_chats() {
    use presage::proto::{AddressableMessage, ConversationIdentifier, addressable_message, conversation_identifier};
    use sync_message::delete_for_me::{ConversationDelete, MessageDeletes};
    let direct = |sid: &str| ConversationIdentifier {
        identifier: Some(conversation_identifier::Identifier::ThreadServiceId(sid.into())),
    };
    let by = |sid: &str, ts: u64| AddressableMessage {
        author: Some(addressable_message::Author::AuthorServiceId(sid.into())),
        sent_timestamp: Some(ts),
    };
    let sync = SyncMessage {
        content: Some(sync_message::Content::DeleteForMe(sync_message::DeleteForMe {
            message_deletes: vec![MessageDeletes {
                conversation: Some(direct(ALICE)),
                messages: vec![by(ALICE, 10), by(ME, 20)],
            }],
            conversation_deletes: vec![ConversationDelete {
                conversation: Some(ConversationIdentifier {
                    identifier: Some(conversation_identifier::Identifier::ThreadGroupId(vec![9; 32])),
                }),
                is_full_delete: Some(true),
                most_recent_messages: vec![by(BOB, 30)],
                ..Default::default()
            }],
            local_only_conversation_deletes: vec![sync_message::delete_for_me::LocalOnlyConversationDelete {
                conversation: Some(direct(BOB)),
            }],
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    let [Action::DeleteForMe { messages, conversations }] = actions.as_slice() else { panic!("{actions:?}") };
    let alice = ThreadRef::Direct(ALICE.into());
    assert_eq!(messages, &[(alice.clone(), Some(ALICE.to_owned()), 10), (alice, None, 20)]);
    assert_eq!(
        conversations,
        &[
            ChatDelete {
                thread: ThreadRef::GroupId(vec![9; 32]),
                most_recent: vec![(Some(BOB.to_owned()), 30)],
                most_recent_non_expiring: vec![],
                full: true,
            },
            ChatDelete { thread: ThreadRef::Direct(BOB.into()), most_recent: vec![], most_recent_non_expiring: vec![], full: true },
        ]
    );
}

#[test]
fn timer_versions_follow_desktop() {
    // A newer or equal version wins; an older or missing one doesn't.
    assert_eq!(timer_update((0, 1), 3600, Some(2), false, false), Some((3600, 2)));
    assert_eq!(timer_update((3600, 2), 60, Some(2), false, false), Some((60, 2)));
    assert_eq!(timer_update((3600, 3), 60, Some(2), false, false), None);
    assert_eq!(timer_update((3600, 3), 0, None, false, false), None);
    // Nothing to change.
    assert_eq!(timer_update((3600, 2), 3600, Some(2), false, false), None);
    // Explicit updates always set the timer, keeping the newer version.
    assert_eq!(timer_update((3600, 5), 0, Some(2), true, false), Some((0, 5)));
    // Groups only through explicit updates.
    assert_eq!(timer_update((0, 1), 60, Some(9), false, true), None);
    assert_eq!(timer_update((0, 1), 60, None, true, true), Some((60, 1)));
}

#[test]
fn messages_carry_the_timer_version() {
    let dm = DataMessage { expire_timer: Some(60), expire_timer_version: Some(3), ..text("hi", 100) };
    let actions = map_content(&content(ALICE, dm), &ctx());
    let [Action::Insert { message, timer_version, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((message.expires_in, *timer_version), (60, Some(3)));
}

#[test]
fn pins_and_unpins_name_their_target() {
    use data_message::{PinMessage, UnpinMessage, pin_message::PinDuration};
    let alice_binary = ServiceId::parse_from_service_id_string(ALICE).unwrap().service_id_binary();
    let me_binary = ServiceId::parse_from_service_id_string(ME).unwrap().service_id_binary();
    let dm = DataMessage {
        timestamp: Some(300),
        pin_message: Some(PinMessage {
            target_author_aci_binary: Some(me_binary),
            target_sent_timestamp: Some(100),
            pin_duration: Some(PinDuration::PinDurationSeconds(86_400)),
        }),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ALICE, dm), &ctx()),
        vec![Action::Pin {
            conversation: ConversationId::direct(ALICE),
            by: Some(ALICE.into()),
            target_author: None,
            target_sent_at: 100,
            duration: Some(86_400),
            at: 300,
        }]
    );
    let dm = DataMessage {
        timestamp: Some(400),
        unpin_message: Some(UnpinMessage { target_author_aci_binary: Some(alice_binary), target_sent_timestamp: Some(100) }),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ALICE, dm), &ctx()),
        vec![Action::Unpin { conversation: ConversationId::direct(ALICE), target_author: Some(ALICE.into()), target_sent_at: 100 }]
    );
}

#[test]
fn mentions_round_trip_through_placeholders() {
    let body = "hi \u{FFFC} and 👋 \u{FFFC}!";
    let acis = vec![ALICE.to_owned(), BOB.to_owned()];
    let (text, spans) = render_mention_spans(body, &acis, &["Alice".into(), "Bob".into()]);
    assert_eq!(text, "hi @Alice and 👋 @Bob!");
    assert_eq!(&text[spans[0].start..spans[0].start + spans[0].len], "@Alice");
    assert_eq!(&text[spans[1].start..spans[1].start + spans[1].len], "@Bob");
    assert_eq!(spans[1].aci, BOB);
    // UTF-16: "hi " 3, placeholder 1, " and " 5, the waving hand 2, space 1.
    let ranges = mention_ranges(body, &acis);
    assert_eq!(ranges.iter().map(|r| r.start.unwrap()).collect::<Vec<_>>(), vec![3, 12]);
    assert_eq!(mentions(&ranges), acis);
    // And back, for sending again.
    assert_eq!(placeholders_for(&text, &spans), (body.to_owned(), acis));
}

#[test]
fn group_changes_become_notices() {
    use presage::libsignal_service::groups_v2::{GroupChange, GroupChanges, Timer};
    use signal_store::{GroupNotice, Notice};
    let editor = presage::libsignal_service::protocol::Aci::parse_from_service_id_string(ALICE).unwrap();
    let me = presage::libsignal_service::protocol::Aci::parse_from_service_id_string(ME).unwrap();
    let changes = GroupChanges {
        group_id: [0; 32],
        editor,
        version: 2,
        changes: vec![
            GroupChange::Title("Hikers".into()),
            GroupChange::Timer(Some(Timer { duration: 3600 })),
            GroupChange::DeleteMember(me),
        ],
        change_epoch: 0,
    };
    let summary = summarize_group_change(&changes, ME);
    let by = Some(ALICE.to_owned());
    assert_eq!(
        summary.notices,
        vec![
            Notice::Group { by: by.clone(), change: GroupNotice::Title { title: "Hikers".into() } },
            Notice::Timer { by: by.clone(), seconds: 3600 },
            Notice::Group { by, change: GroupNotice::Removed { member: None } },
        ]
    );
    assert_eq!(summary.timer, Some(3600));
    assert!(summary.left, "we were removed");
}

#[test]
fn sync_verified() {
    let sync = |state: presage::proto::verified::State| SyncMessage {
        content: Some(sync_message::Content::Verified(presage::proto::Verified {
            destination_aci: Some(ALICE.into()),
            identity_key: Some(vec![5; 33]),
            state: Some(state.into()),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ME, sync(presage::proto::verified::State::Verified)), &ctx()),
        vec![Action::Verified { aci: ALICE.into(), identity_key: vec![5; 33], verified: true }]
    );
    assert_eq!(
        map_content(&content(ME, sync(presage::proto::verified::State::Default)), &ctx()),
        vec![Action::Verified { aci: ALICE.into(), identity_key: vec![5; 33], verified: false }]
    );
}

#[test]
fn sync_call_event() {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let event = |kind: Type, direction: Direction, event: Event| SyncMessage {
        content: Some(sync_message::Content::CallEvent(sync_message::CallEvent {
            conversation_id: Some(presage::libsignal_service::protocol::Aci::parse_from_service_id_string(ALICE).unwrap().service_id_binary()),
            call_id: Some(u64::MAX),
            timestamp: Some(500),
            r#type: Some(kind.into()),
            direction: Some(direction.into()),
            event: Some(event.into()),
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, event(Type::VideoCall, Direction::Incoming, Event::Accepted)), &ctx());
    assert_eq!(
        actions,
        vec![Action::CallEvent(CallEventSync {
            peer: ALICE.into(),
            call_id: -1,
            timestamp: 500,
            video: true,
            incoming: true,
            outcome: Some(signal_store::CallOutcome::Accepted),
        })]
    );
    let declined = map_content(&content(ME, event(Type::AudioCall, Direction::Incoming, Event::NotAccepted)), &ctx());
    assert!(matches!(&declined[..], [Action::CallEvent(e)] if e.outcome == Some(signal_store::CallOutcome::Declined)));
    let deleted = map_content(&content(ME, event(Type::AudioCall, Direction::Outgoing, Event::Delete)), &ctx());
    assert!(matches!(&deleted[..], [Action::CallEvent(e)] if e.outcome.is_none()));
    // Group calls aren't recorded this way.
    assert!(map_content(&content(ME, event(Type::GroupCall, Direction::Incoming, Event::Accepted)), &ctx()).is_empty());
}

#[test]
fn group_call_updates_become_notices() {
    let mut dm = DataMessage { timestamp: Some(900), ..Default::default() };
    dm.group_v2 = Some(GroupContextV2 { master_key: Some(group_key()), ..Default::default() });
    dm.group_call_update = Some(data_message::GroupCallUpdate { era_id: Some("era".into()) });
    assert_eq!(
        map_content(&content(ALICE, dm), &ctx()),
        vec![Action::GroupCallStarted {
            conversation: ConversationId::group(&group_key()),
            by: Some(ALICE.into()),
            era: Some("era".into()),
            at: 900
        }]
    );
}
