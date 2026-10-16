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

use crate::remote_config::RemoteConfig;

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
            styles: vec![],
            long_text: None,
        }]
    );
}

#[test]
fn bodies_over_2_kib_are_dropped_as_desktop_drops_them() {
    let limit = crate::long_text::MAX_BODY_BYTES;
    let long = "a".repeat(limit + 1);
    assert_eq!(map_content(&content(ALICE, text(&long, 300)), &ctx()), vec![]);
    let edit = EditMessage { target_sent_timestamp: Some(100), data_message: Some(text(&long, 300)) };
    assert_eq!(map_content(&content(ALICE, edit), &ctx()), vec![]);
    let sync = |message: Option<DataMessage>, edit_message: Option<EditMessage>| SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            destination_service_id: Some(ALICE.into()),
            timestamp: Some(700),
            message,
            edit_message,
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(map_content(&content(ME, sync(Some(text(&long, 700)), None)), &ctx()), vec![]);
    let edit = EditMessage { target_sent_timestamp: Some(100), data_message: Some(text(&long, 700)) };
    assert_eq!(map_content(&content(ME, sync(None, Some(edit))), &ctx()), vec![]);
    // Exactly 2 KiB is fine.
    let most = "a".repeat(limit);
    assert_eq!(map_content(&content(ALICE, text(&most, 300)), &ctx()).len(), 1);
    assert_eq!(map_content(&content(ME, sync(Some(text(&most, 700)), None)), &ctx()).len(), 1);
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
            styles: vec![],
            long_text: None,
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
fn fetch_latest_requests_a_resync_or_our_profile() {
    let fetch = |kind: sync_message::fetch_latest::Type| SyncMessage {
        content: Some(sync_message::Content::FetchLatest(sync_message::FetchLatest { r#type: Some(kind as i32) })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, fetch(sync_message::fetch_latest::Type::StorageManifest)), &ctx());
    assert!(matches!(actions.as_slice(), [Action::StorageChanged]), "{actions:?}");
    let actions = map_content(&content(ME, fetch(sync_message::fetch_latest::Type::LocalProfile)), &ctx());
    assert!(matches!(actions.as_slice(), [Action::LocalProfileChanged]), "{actions:?}");
    let actions = map_content(&content(ME, fetch(sync_message::fetch_latest::Type::SubscriptionStatus)), &ctx());
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn configuration_syncs_carry_privacy_settings_as_desktop_reads_them() {
    let configuration = |config: sync_message::Configuration| SyncMessage {
        content: Some(sync_message::Content::Configuration(config)),
        ..Default::default()
    };
    let all = sync_message::Configuration {
        read_receipts: Some(true),
        unidentified_delivery_indicators: Some(true),
        typing_indicators: Some(false),
        link_previews: Some(true),
    };
    assert_eq!(
        map_content(&content(ME, configuration(all)), &ctx()),
        vec![Action::AccountSettings(crate::AccountSettings {
            read_receipts: Some(true),
            typing_indicators: Some(false),
            link_previews: Some(true),
            sealed_sender_indicators: None,
        })]
    );
    // Read receipts left out are off (Desktop's `Boolean(readReceipts)`);
    // the others stay as they are.
    assert_eq!(
        map_content(&content(ME, configuration(Default::default())), &ctx()),
        vec![Action::AccountSettings(crate::AccountSettings { read_receipts: Some(false), ..Default::default() })]
    );
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
    let ranges = mention_ranges(body, &acis, &RemoteConfig::default());
    assert_eq!(ranges.iter().map(|r| r.start.unwrap()).collect::<Vec<_>>(), vec![3, 12]);
    assert_eq!(mentions(&ranges), acis);
    // With `binaryServiceId` on, the binary field, read back the same.
    let binary = RemoteConfig::from_pairs([("desktop.binaryServiceId.prod", "8.0.0")]);
    let ranges = mention_ranges(body, &acis, &binary);
    assert!(matches!(ranges[0].associated_value, Some(body_range::AssociatedValue::MentionAciBinary(_))));
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
    let summary = summarize_group_change(&changes, ME, None, &Default::default());
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
    assert!(!summary.ended);
}

#[test]
fn ending_a_group_is_noticed() {
    use presage::libsignal_service::groups_v2::{GroupChange, GroupChanges};
    use signal_store::{GroupNotice, Notice};
    let editor = presage::libsignal_service::protocol::Aci::parse_from_service_id_string(ALICE).unwrap();
    let changes =
        GroupChanges { group_id: [0; 32], editor, version: 3, changes: vec![GroupChange::TerminateGroup], change_epoch: 7 };
    let summary = summarize_group_change(&changes, ME, None, &Default::default());
    assert!(summary.ended);
    assert!(!summary.left, "ending it doesn't take us out");
    assert_eq!(summary.notices, vec![Notice::Group { by: Some(ALICE.into()), change: GroupNotice::Ended }]);
}

/// A change by `editor` (an ACI string).
fn change_by(editor: &str, changes: Vec<presage::libsignal_service::groups_v2::GroupChange>) -> presage::libsignal_service::groups_v2::GroupChanges {
    let editor = presage::libsignal_service::protocol::Aci::parse_from_service_id_string(editor).unwrap();
    presage::libsignal_service::groups_v2::GroupChanges { group_id: [0; 32], editor, version: 4, changes, change_epoch: 5 }
}

fn aci_of(sid: &str) -> presage::libsignal_service::protocol::Aci {
    presage::libsignal_service::protocol::Aci::parse_from_service_id_string(sid).unwrap()
}

#[test]
fn invitations_read_as_desktop_diffs_them() {
    use presage::libsignal_service::groups_v2::{GroupChange, PendingMember, Role};
    use signal_store::{GroupInvite, GroupNotice, GroupState, Inviter, Notice};
    const PNI: &str = "PNI:44444444-4444-4444-8444-444444444444";
    let pending = |sid: &str| {
        GroupChange::NewPendingMember(PendingMember {
            address: ServiceId::parse_from_service_id_string(sid).unwrap(),
            role: Role::Default,
            added_by_aci: aci_of(ALICE),
            timestamp: 0,
        })
    };
    // One invitation (to our PNI): we're invited, by Alice.
    let summary = summarize_group_change(&change_by(ALICE, vec![pending(PNI)]), ME, Some(PNI), &GroupState::default());
    let by = Some(ALICE.to_owned());
    assert_eq!(summary.notices, vec![Notice::Group { by: by.clone(), change: GroupNotice::Invited { member: None } }]);
    assert_eq!(summary.membership, Some(MembershipChange::Invited { by: ALICE.into() }));
    // Several: counted.
    let summary = summarize_group_change(&change_by(ALICE, vec![pending(BOB), pending(PNI)]), ME, None, &GroupState::default());
    assert_eq!(summary.notices, vec![Notice::Group { by: by.clone(), change: GroupNotice::InvitedMany { count: 2 } }]);

    // Bob declines the invitation we sent; he accepts another.
    let before = GroupState { invites: vec![GroupInvite { service_id: BOB.into(), by: ME.into() }], ..Default::default() };
    let declined = GroupChange::DeletePendingMember(ServiceId::parse_from_service_id_string(BOB).unwrap());
    let summary = summarize_group_change(&change_by(BOB, vec![declined]), ME, None, &before);
    assert_eq!(
        summary.notices,
        vec![Notice::Group { by: Some(BOB.into()), change: GroupNotice::InviteRemoved { member: Some(BOB.into()), inviter: Inviter::Us } }]
    );
    let accepted = GroupChange::PromotePendingMember {
        address: ServiceId::parse_from_service_id_string(BOB).unwrap(),
        profile_key: presage::libsignal_service::prelude::ProfileKey::create([1; 32]),
    };
    let summary = summarize_group_change(&change_by(BOB, vec![accepted]), ME, None, &before);
    assert_eq!(
        summary.notices,
        vec![Notice::Group { by: Some(BOB.into()), change: GroupNotice::AcceptedInvite { member: Some(BOB.into()), inviter: Inviter::Us } }]
    );
    // Our own invitation revoked: we're out.
    let revoked = GroupChange::DeletePendingMember(ServiceId::parse_from_service_id_string(ME).unwrap());
    let summary = summarize_group_change(&change_by(ALICE, vec![revoked]), ME, None, &GroupState::default());
    assert_eq!(summary.membership, Some(MembershipChange::Out));
}

#[test]
fn requests_links_and_bans_read_as_desktop_diffs_them() {
    use presage::libsignal_service::groups_v2::{AccessRequired, GroupChange, RequestingMember, Role};
    use signal_store::{GroupLink, GroupNotice, GroupState, Notice};
    let by = |sid: &str| Some(sid.to_owned());
    let requesting = GroupChange::NewRequestingMember(RequestingMember {
        aci: aci_of(ME),
        profile_key: presage::libsignal_service::prelude::ProfileKey::create([1; 32]),
        timestamp: 0,
    });
    let summary = summarize_group_change(&change_by(ME, vec![requesting]), ME, None, &GroupState::default());
    assert_eq!(summary.notices, vec![Notice::Group { by: None, change: GroupNotice::Requested { member: None } }]);
    assert_eq!(summary.membership, Some(MembershipChange::Requesting));
    let approved = GroupChange::PromoteRequestingMember { aci: aci_of(ME), role: Role::Default };
    let summary = summarize_group_change(&change_by(ALICE, vec![approved]), ME, None, &GroupState::default());
    assert_eq!(summary.notices, vec![Notice::Group { by: by(ALICE), change: GroupNotice::Approved { member: None } }]);
    assert_eq!(summary.membership, Some(MembershipChange::Joined));

    // Turned on with a new password: one notice, not a reset too.
    let off = GroupState::default();
    let on = GroupState { link: Some(GroupLink { url: "u".into(), approval: false }), ..Default::default() };
    let turn_on = vec![GroupChange::InviteLinkAccess(AccessRequired::Any), GroupChange::InviteLinkPassword("cA==".into())];
    let summary = summarize_group_change(&change_by(ALICE, turn_on), ME, None, &off);
    assert_eq!(summary.notices, vec![Notice::Group { by: by(ALICE), change: GroupNotice::LinkOn { approval: false } }]);
    let summary = summarize_group_change(&change_by(ALICE, vec![GroupChange::InviteLinkPassword("cQ==".into())]), ME, None, &on);
    assert_eq!(summary.notices, vec![Notice::Group { by: by(ALICE), change: GroupNotice::LinkReset }]);
    let approval = vec![GroupChange::InviteLinkAccess(AccessRequired::Administrator)];
    let summary = summarize_group_change(&change_by(ALICE, approval), ME, None, &on);
    assert_eq!(summary.notices, vec![Notice::Group { by: by(ALICE), change: GroupNotice::LinkApproval { on: true } }]);
    let summary = summarize_group_change(&change_by(ALICE, vec![GroupChange::InviteLinkAccess(AccessRequired::Unsatisfiable)]), ME, None, &on);
    assert_eq!(summary.notices, vec![Notice::Group { by: by(ALICE), change: GroupNotice::LinkOff }]);

    // Removing Bob bans him (no notice for the ban), and announcement-only
    // is kept here since presage doesn't.
    let removed = vec![
        GroupChange::DeleteMember(aci_of(BOB)),
        GroupChange::AnnouncementOnly(true),
    ];
    let mut summary = summarize_group_change(&change_by(ALICE, removed), ME, None, &on);
    summary.banned.push(BOB.into());
    let mut state = on.clone();
    apply_to_group_state(&summary, &mut state);
    assert!(state.announcements_only);
    assert_eq!(state.banned, vec![BOB.to_owned()]);
    // Someone adding themselves came by the link.
    let joined = GroupChange::NewMember(presage::libsignal_service::groups_v2::Member {
        aci: aci_of(BOB),
        role: Role::Default,
        profile_key: presage::libsignal_service::prelude::ProfileKey::create([2; 32]),
        joined_at_version: 4,
        label: None,
        label_emoji: None,
    });
    let summary = summarize_group_change(&change_by(BOB, vec![joined]), ME, None, &on);
    assert_eq!(summary.notices, vec![Notice::Group { by: by(BOB), change: GroupNotice::JoinedByLink { member: by(BOB) } }]);
}

#[test]
fn group_state_comes_from_presages_copy_and_what_it_lacks() {
    use presage::libsignal_service::groups_v2::{AccessRequired, Role};
    use presage::model::groups::{AccessControl, Group, Member, PendingMember, RequestingMember};
    use signal_store::{GroupState, Membership};
    let member = |sid: &str, role| Member {
        aci: aci_of(sid),
        role,
        profile_key: presage::libsignal_service::prelude::ProfileKey::create([1; 32]),
        joined_at_revision: 0,
        label: None,
        label_emoji: None,
    };
    let group = Group {
        title: "Hikers".into(),
        avatar: String::new(),
        disappearing_messages_timer: None,
        access_control: Some(AccessControl {
            attributes: AccessRequired::Administrator,
            members: AccessRequired::Member,
            add_from_invite_link: AccessRequired::Administrator,
            member_label: AccessRequired::Member,
        }),
        revision: 3,
        members: vec![member(ALICE, Role::Administrator)],
        pending_members: vec![PendingMember {
            uuid: aci_of(ME).into(),
            service_id_type: Default::default(),
            role: Role::Default,
            added_by_aci: aci_of(ALICE),
            timestamp: 0,
        }],
        requesting_members: vec![RequestingMember {
            aci: aci_of(BOB),
            profile_key: presage::libsignal_service::prelude::ProfileKey::create([1; 32]),
            timestamp: 9,
        }],
        invite_link_password: vec![7; 16],
        description: None,
    };
    let stored = GroupState { announcements_only: true, banned: vec!["x".into()], ..Default::default() };
    let state = group_state_of(&group, [3; 32], ME, None, &stored);
    assert!(state.info_admins_only && !state.members_admins_only);
    assert!(state.announcements_only && state.banned == ["x"], "kept from what's stored");
    let link = state.link.expect("the link is on");
    assert!(link.approval);
    assert_eq!(crate::group_admin::parse_link(&link.url), Ok(([3; 32], vec![7; 16])));
    assert_eq!(state.requests[0].aci, BOB);
    assert_eq!(state.membership, Membership::Invited { by: Some(ALICE.into()) });
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
}

#[test]
fn sync_group_call_events() {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let event = |direction: Direction, event: Event| SyncMessage {
        content: Some(sync_message::Content::CallEvent(sync_message::CallEvent {
            conversation_id: Some(vec![9; 32]),
            call_id: Some(7),
            timestamp: Some(400),
            r#type: Some(Type::GroupCall.into()),
            direction: Some(direction.into()),
            event: Some(event.into()),
        })),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ME, event(Direction::Incoming, Event::Accepted)), &ctx()),
        vec![Action::GroupCallEvent(GroupCallEventSync { group_id: vec![9; 32], call_id: 7, timestamp: 400, incoming: true, accepted: true })]
    );
    let deleted = map_content(&content(ME, event(Direction::Outgoing, Event::Delete)), &ctx());
    assert!(matches!(&deleted[..], [Action::GroupCallEvent(e)] if !e.accepted && !e.incoming));
    // Desktop refuses "not accepted" and "observed" for a group call.
    assert!(map_content(&content(ME, event(Direction::Incoming, Event::NotAccepted)), &ctx()).is_empty());
    assert!(map_content(&content(ME, event(Direction::Incoming, Event::Observed)), &ctx()).is_empty());
}

#[test]
fn sync_call_log_events() {
    use presage::proto::sync_message::call_log_event::Type;
    let event = |kind: Type, call_id: Option<u64>| SyncMessage {
        content: Some(sync_message::Content::CallLogEvent(sync_message::CallLogEvent {
            r#type: Some(kind.into()),
            timestamp: Some(300),
            conversation_id: Some(vec![1; 16]),
            call_id,
        })),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ME, event(Type::Clear, Some(u64::MAX))), &ctx()),
        vec![Action::CallLogEvent(CallLogEventSync {
            kind: CallLogKind::Clear,
            conversation: Some(vec![1; 16]),
            call_id: Some(-1),
            timestamp: 300,
            at: 500,
        })]
    );
    let read = map_content(&content(ME, event(Type::MarkedAsReadInConversation, None)), &ctx());
    assert!(matches!(&read[..], [Action::CallLogEvent(e)] if e.kind == CallLogKind::MarkedAsReadInConversation && e.call_id.is_none()));
    let read = map_content(&content(ME, event(Type::MarkedAsRead, None)), &ctx());
    assert!(matches!(&read[..], [Action::CallLogEvent(e)] if e.kind == CallLogKind::MarkedAsRead));
    // Desktop doesn't clear one chat's calls either.
    assert!(map_content(&content(ME, event(Type::ClearInConversation, None)), &ctx()).is_empty());
}

#[test]
fn sync_adhoc_call_events_and_call_link_updates() {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let event = |event: Event| SyncMessage {
        content: Some(sync_message::Content::CallEvent(sync_message::CallEvent {
            conversation_id: Some(vec![0xab, 0xcd]),
            call_id: Some(7),
            timestamp: Some(500),
            r#type: Some(Type::AdHocCall.into()),
            direction: Some(Direction::Incoming.into()),
            event: Some(event.into()),
        })),
        ..Default::default()
    };
    let joined = AdhocCallSync { room_id: "abcd".into(), call_id: 7, timestamp: 500, status: Some(signal_store::AdhocCallStatus::Joined) };
    assert_eq!(map_content(&content(ME, event(Event::Accepted)), &ctx()), vec![Action::AdhocCallEvent(joined.clone())]);
    let observed = map_content(&content(ME, event(Event::Observed)), &ctx());
    assert!(matches!(&observed[..], [Action::AdhocCallEvent(e)] if e.status == Some(signal_store::AdhocCallStatus::Generic)));
    let deleted = map_content(&content(ME, event(Event::Delete)), &ctx());
    assert!(matches!(&deleted[..], [Action::AdhocCallEvent(e)] if e.status.is_none()));
    assert!(map_content(&content(ME, event(Event::NotAccepted)), &ctx()).is_empty());

    let update = |admin: Option<Vec<u8>>| SyncMessage {
        content: Some(sync_message::Content::CallLinkUpdate(sync_message::CallLinkUpdate {
            root_key: Some(vec![1; 16]),
            admin_passkey: admin,
            r#type: None,
        })),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ME, update(Some(vec![2; 16]))), &ctx()),
        vec![Action::CallLinkUpdate { root_key: vec![1; 16], admin_key: Some(vec![2; 16]) }]
    );
    // An empty passkey is none.
    assert_eq!(
        map_content(&content(ME, update(Some(Vec::new()))), &ctx()),
        vec![Action::CallLinkUpdate { root_key: vec![1; 16], admin_key: None }]
    );
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

#[test]
fn styles_land_where_they_were_after_mentions_and_emoji() {
    use presage::proto::body_range::{AssociatedValue, Style};
    use signal_store::{StyleSpan, TextStyle};
    // "😀 hi ￼ bold": the emoji is two UTF-16 units, the mention one.
    let body = "😀 hi \u{FFFC} bold";
    let range = |start, length, style: Style| presage::proto::BodyRange {
        start: Some(start),
        length: Some(length),
        associated_value: Some(AssociatedValue::Style(style.into())),
    };
    let ranges = [range(3, 2, Style::Italic), range(6, 1, Style::Spoiler), range(8, 4, Style::Bold)];
    let wire = styles(&ranges);
    let (text, mentions, spans) = render_body(body, &[ALICE.to_owned()], &["Alice".to_owned()], &wire);
    assert_eq!(text, "😀 hi @Alice bold");
    assert_eq!(mentions[0].start, "😀 hi ".len());
    let at = |s: &StyleSpan| &text[s.start..s.start + s.len];
    assert_eq!(spans.iter().map(|s| (at(s), s.style)).collect::<Vec<_>>(), [
        ("hi", TextStyle::Italic),
        ("@Alice", TextStyle::Spoiler),
        ("bold", TextStyle::Bold),
    ]);
    // And back out: byte spans over the body as sent become the same ranges.
    let sent = [
        StyleSpan { start: "😀 ".len(), len: 2, style: TextStyle::Italic },
        StyleSpan { start: "😀 hi ".len(), len: '\u{FFFC}'.len_utf8(), style: TextStyle::Spoiler },
        StyleSpan { start: body.len() - 4, len: 4, style: TextStyle::Bold },
    ];
    assert_eq!(style_ranges(&wire_styles(body, &sent)), ranges);
}

#[test]
fn quoted_spoilers_stay_hidden() {
    use presage::proto::body_range::{AssociatedValue, Style};
    let mut dm = DataMessage { body: Some("ok".into()), timestamp: Some(500), ..Default::default() };
    dm.quote = Some(presage::proto::data_message::Quote {
        id: Some(400),
        author_aci: Some(ALICE.into()),
        text: Some("it was the butler".into()),
        body_ranges: vec![presage::proto::BodyRange {
            start: Some(7),
            length: Some(10),
            associated_value: Some(AssociatedValue::Style(Style::Spoiler.into())),
        }],
        ..Default::default()
    });
    let actions = map_data_message(&dm, Origin::Incoming { sender: ALICE.into() }, 500, &ctx());
    let Some(Action::Insert { message, .. }) = actions.first() else { panic!("{actions:?}") };
    assert_eq!(message.quote.as_ref().unwrap().text, "it was ■■■■");
}

fn text_story(text: &str) -> presage::proto::StoryMessage {
    use presage::proto::{StoryMessage, TextAttachment, story_message, text_attachment};
    StoryMessage {
        attachment: Some(story_message::Attachment::TextAttachment(TextAttachment {
            text: Some(text.into()),
            text_style: Some(text_attachment::Style::Serif.into()),
            text_foreground_color: Some(0xff00_0000),
            background: Some(text_attachment::Background::Gradient(text_attachment::Gradient {
                colors: vec![0xff11_2233, 0xff44_5566],
                positions: vec![0., 1.],
                angle: Some(180),
                ..Default::default()
            })),
            ..Default::default()
        })),
        allows_replies: Some(true),
        ..Default::default()
    }
}

#[test]
fn stories_arrive_as_stories_not_messages() {
    use signal_store::{StoryBackground, StoryContent, StoryFont};
    let actions = map_content(&content(ALICE, ContentBody::StoryMessage(text_story("Hello"))), &ctx());
    let [Action::Story { story, pointer: None }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((story.author.as_deref(), story.sent_at, story.group.as_ref(), story.allows_replies), (Some(ALICE), 500, None, true));
    let StoryContent::Text(text) = &story.content else { panic!() };
    assert_eq!((text.text.as_str(), text.font, text.foreground), ("Hello", StoryFont::Serif, Some(0xff00_0000)));
    assert_eq!(text.background, StoryBackground::Gradient { colors: vec![0xff11_2233, 0xff44_5566], positions: vec![0., 1.], angle: 180 });

    // Empty, with no link: dropped, as Desktop does.
    assert!(map_content(&content(ALICE, ContentBody::StoryMessage(text_story(" "))), &ctx()).is_empty());

    // A photo story to a group.
    let mut photo = presage::proto::StoryMessage {
        attachment: Some(presage::proto::story_message::Attachment::FileAttachment(AttachmentPointer {
            content_type: Some("image/jpeg".into()),
            caption: Some("Look".into()),
            ..Default::default()
        })),
        ..Default::default()
    };
    photo.group = Some(GroupContextV2 { master_key: Some(group_key()), ..Default::default() });
    let actions = map_content(&content(ALICE, ContentBody::StoryMessage(photo)), &ctx());
    let [Action::Story { story, pointer: Some(_) }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!(story.group, Some(ConversationId::group(&group_key())));
    assert!(matches!(&story.content, StoryContent::Media { caption, .. } if caption == "Look"));
}

#[test]
fn our_stories_from_another_device_are_ours() {
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            timestamp: Some(700),
            story_message: Some(text_story("Mine")),
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    let [Action::Story { story, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((story.author.as_deref(), story.sent_at, story.read), (None, 700, true));
}

#[test]
fn story_replies_go_with_the_story_in_groups_and_to_the_chat_one_to_one() {
    let context = data_message::StoryContext { author_aci: Some(BOB.into()), sent_timestamp: Some(400), ..Default::default() };
    // In a group: kept with Bob's story, not in the group's timeline.
    let mut reply = text("nice!", 600);
    reply.story_context = Some(context.clone());
    reply.group_v2 = Some(GroupContextV2 { master_key: Some(group_key()), ..Default::default() });
    let actions = map_content(&content(ALICE, reply), &ctx());
    let [Action::StoryReply { story_author, story_sent_at, reply, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((story_author.as_deref(), *story_sent_at, reply.body.as_str(), reply.emoji.as_ref()), (Some(BOB), 400, "nice!", None));

    // One to one, to our story: a message quoting it.
    let mut reply = text("love it", 610);
    reply.story_context = Some(data_message::StoryContext { author_aci: Some(ME.into()), sent_timestamp: Some(300), ..Default::default() });
    let actions = map_content(&content(ALICE, reply), &ctx());
    let [Action::StoryReply { story_author: None, story_sent_at: 300, reply: kept, .. }, Action::Insert { message, .. }] =
        actions.as_slice()
    else {
        panic!("{actions:?}")
    };
    assert_eq!((kept.author.as_deref(), kept.body.as_str()), (Some(ALICE), "love it"), "kept with our story too");
    let quote = message.quote.as_ref().unwrap();
    assert_eq!((message.body.as_str(), quote.story, quote.author.as_ref(), quote.sent_at, quote.text.as_str()), ("love it", true, None, 300, "Story"));

    // Ours to Bob's story, from another device: only the chat message.
    let mut ours = text("ha", 615);
    ours.story_context = Some(context.clone());
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            destination_service_id: Some(BOB.into()),
            timestamp: Some(615),
            message: Some(ours),
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    assert!(matches!(actions.as_slice(), [Action::Insert { .. }]), "{actions:?}");

    // A reaction to it: the emoji, "Reacted to your story" (not a lost
    // reaction to a message).
    let mut reaction = DataMessage { timestamp: Some(620), ..Default::default() };
    reaction.story_context = Some(data_message::StoryContext { author_aci: Some(ME.into()), sent_timestamp: Some(300), ..Default::default() });
    reaction.reaction = Some(data_message::Reaction {
        emoji: Some("🔥".into()),
        target_author_aci: Some(ME.into()),
        target_sent_timestamp: Some(300),
        ..Default::default()
    });
    let actions = map_content(&content(ALICE, reaction), &ctx());
    let [Action::StoryReply { reply, .. }, Action::Insert { message, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!(reply.emoji.as_deref(), Some("🔥"));
    assert_eq!((message.body.as_str(), message.quote.as_ref().unwrap().text.as_str()), ("🔥", "Reacted to your story"));
}

#[test]
fn our_stories_keep_who_they_went_to_and_recipient_updates() {
    let recipient = |sid: &str, lists: &[&str], replies| sync_message::sent::StoryMessageRecipient {
        destination_service_id: Some(sid.into()),
        distribution_list_ids: lists.iter().map(|l| (*l).to_owned()).collect(),
        is_allowed_to_reply: Some(replies),
        ..Default::default()
    };
    let binary = sync_message::sent::StoryMessageRecipient {
        destination_service_id_binary: Some(ServiceId::parse_from_service_id_string(BOB).unwrap().service_id_binary()),
        distribution_list_ids: vec!["list".into()],
        ..Default::default()
    };
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            timestamp: Some(700),
            story_message: Some(text_story("Mine")),
            story_message_recipients: vec![recipient(ALICE, &["my", "list"], true), binary],
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    let [Action::Story { story, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    let kept: Vec<_> = story.recipients.iter().map(|r| (r.service_id.as_str(), r.list_ids.len(), r.allows_replies)).collect();
    assert_eq!(kept, [(ALICE, 2, true), (BOB, 1, false)]);

    // Deleted on the phone: a recipient update, with no story.
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            destination_service_id: Some(ME.into()),
            timestamp: Some(700),
            story_message_recipients: vec![recipient(ALICE, &[], true)],
            is_recipient_update: Some(true),
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    let [Action::StoryRecipientUpdate { sent_at: 700, recipients }] = actions.as_slice() else { panic!("{actions:?}") };
    assert!(recipients[0].list_ids.is_empty());
}

#[test]
fn viewed_receipts_count_story_views() {
    let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Viewed.into()), timestamp: vec![300] };
    let actions = map_content(&content(ALICE, receipt), &ctx());
    assert!(actions.iter().any(|a| matches!(a, Action::StoryViewed { viewer, sent_at, at: 500 } if viewer == ALICE && sent_at == &[300])));
    let sync = SyncMessage {
        viewed: vec![sync_message::Viewed { sender_aci: Some(ALICE.into()), timestamp: Some(400), ..Default::default() }],
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    assert!(actions.iter().any(|a| matches!(a, Action::ViewedSync(v) if v == &[(Some(ALICE.into()), 400)])));
}

fn long_text_pointer() -> AttachmentPointer {
    AttachmentPointer {
        content_type: Some(LONG_TEXT_TYPE.into()),
        attachment_identifier: Some(attachment_pointer::AttachmentIdentifier::CdnKey("text-key".into())),
        cdn_number: Some(3),
        key: Some(vec![9; 64]),
        digest: Some(vec![8; 32]),
        size: Some(3000),
        ..Default::default()
    }
}

#[test]
fn a_long_text_is_kept_apart_from_the_attachments() {
    // As Desktop sends one: the whole text first, the body only its start.
    let photo = AttachmentPointer { content_type: Some("image/png".into()), ..Default::default() };
    let mut dm = text("the start", 100);
    dm.attachments = vec![long_text_pointer(), photo.clone()];
    dm.body_ranges = vec![BodyRange {
        start: Some(0),
        length: Some(2500),
        associated_value: Some(body_range::AssociatedValue::Style(body_range::Style::Bold.into())),
    }];
    let actions = map_content(&content(ALICE, dm), &ctx());
    let [Action::Insert { message, pointers, long_text, styles, .. }] = actions.as_slice() else {
        panic!("{actions:?}")
    };
    assert_eq!(message.body, "the start");
    assert_eq!(pointers, &[photo]);
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].content_type, "image/png");
    assert_eq!(long_text.as_ref(), Some(&long_text_pointer()));
    // Its ranges are over the whole text, kept as sent.
    assert_eq!(styles, &[WireStyle { start: 0, len: 2500, style: TextStyle::Bold }]);

    // An edit can bring one too.
    let mut edited = text("the new start", 300);
    edited.attachments = vec![long_text_pointer()];
    let edit = EditMessage { target_sent_timestamp: Some(100), data_message: Some(edited) };
    let actions = map_content(&content(ALICE, edit), &ctx());
    let [Action::Edit { long_text, body, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((body.as_str(), long_text.as_ref()), ("the new start", Some(&long_text_pointer())));

    // Only the stored message's own attachments are counted for downloads.
    let mut dm = text("x", 1);
    dm.attachments = vec![long_text_pointer(), AttachmentPointer { size: Some(5), ..Default::default() }];
    assert_eq!(normal_attachments(&dm).map(|p| p.size).collect::<Vec<_>>(), [Some(5)]);
}

#[test]
fn where_a_long_text_downloads_from() {
    let remote = remote_of(&long_text_pointer()).unwrap();
    assert_eq!((remote.cdn, remote.cdn_key.as_deref()), (3, Some("text-key")));
    assert_eq!((remote.key, remote.digest), ("09".repeat(64), Some("08".repeat(32))));
    // Older pointers name the file by number, on CDN 0.
    let old = AttachmentPointer {
        attachment_identifier: Some(attachment_pointer::AttachmentIdentifier::CdnId(42)),
        cdn_number: None,
        ..long_text_pointer()
    };
    let remote = remote_of(&old).unwrap();
    assert_eq!((remote.cdn, remote.cdn_key.as_deref()), (0, Some("42")));
    // Without its digest it can't be checked, so it isn't fetched.
    assert_eq!(remote_of(&AttachmentPointer { digest: None, ..long_text_pointer() }), None);
}

#[test]
fn formatting_past_the_end_of_the_text_is_cut_at_the_end() {
    // The start of a long text, shown until the rest comes.
    let styles = [WireStyle { start: 4, len: 100, style: TextStyle::Bold }];
    let (body, _, spans) = render_body("the start", &[], &[], &styles);
    assert_eq!(&body[spans[0].start..spans[0].start + spans[0].len], "start");
}

#[test]
fn group_revisions_come_from_messages_and_our_transcripts() {
    let mut dm = text("hi", 100);
    assert_eq!(group_revision_of(&content(ALICE, dm.clone())), None);
    dm.group_v2 = Some(GroupContextV2 { master_key: Some(group_key()), revision: Some(4), ..Default::default() });
    let key: [u8; 32] = group_key().try_into().unwrap();
    assert_eq!(group_revision_of(&content(ALICE, dm.clone())), Some((key, 4)));
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            timestamp: Some(100),
            message: Some(dm),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(group_revision_of(&content(ME, sync)), Some((key, 4)));
}

#[test]
fn admin_deletes_name_the_author_and_the_admin() {
    let aci = |s: &str| presage::libsignal_service::protocol::Aci::parse_from_service_id_string(s).unwrap().service_id_binary();
    let delete = |author: &str| DataMessage {
        timestamp: Some(900),
        group_v2: Some(GroupContextV2 { master_key: Some(group_key()), revision: Some(2), ..Default::default() }),
        admin_delete: Some(data_message::AdminDelete {
            target_author_aci_binary: Some(aci(author)),
            target_sent_timestamp: Some(100),
        }),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(BOB, delete(ALICE)), &ctx()),
        vec![Action::AdminDelete {
            conversation: ConversationId::group(&group_key()),
            target_author: Some(ALICE.into()),
            target_sent_at: 100,
            by: Some(BOB.into()),
            sent_at: 900,
        }]
    );
    // Ours, deleted by an admin; and one we sent from another device.
    let actions = map_content(&content(BOB, delete(ME)), &ctx());
    assert!(matches!(&actions[..], [Action::AdminDelete { target_author: None, by: Some(_), .. }]), "{actions:?}");
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            timestamp: Some(900),
            message: Some(delete(ALICE)),
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    assert!(matches!(&actions[..], [Action::AdminDelete { by: None, target_author: Some(_), .. }]), "{actions:?}");
    // Without its author it names nothing.
    let mut bare = delete(ALICE);
    bare.admin_delete.as_mut().unwrap().target_author_aci_binary = None;
    assert!(map_content(&content(BOB, bare), &ctx()).is_empty());
}

#[test]
fn polls_votes_and_ends_follow_desktop() {
    use data_message::{PollCreate, PollTerminate, PollVote};
    let group = || Some(GroupContextV2 { master_key: Some(group_key()), revision: Some(1), ..Default::default() });
    let create = |question: &str, options: &[&str]| DataMessage {
        timestamp: Some(100),
        group_v2: group(),
        poll_create: Some(PollCreate {
            question: Some(question.into()),
            allow_multiple: Some(true),
            options: options.iter().map(|o| o.to_string()).collect(),
        }),
        ..Default::default()
    };
    let actions = map_content(&content(ALICE, create("Lunch?", &["Tacos", "Pho"])), &ctx());
    let [Action::Insert { message, poll: Some(poll), .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!(message.body, "Lunch?", "the question is the body, for search");
    assert_eq!((poll.options.len(), poll.allow_multiple, poll.ended), (2, true, false));
    // One option, or an empty question: Desktop drops the message.
    assert!(map_content(&content(ALICE, create("Lunch?", &["Tacos"])), &ctx()).is_empty());
    assert!(map_content(&content(ALICE, create("", &["Tacos", "Pho"])), &ctx()).is_empty());

    let alice = ServiceId::parse_from_service_id_string(ALICE).unwrap().service_id_binary();
    let vote = |indexes: Vec<u32>| DataMessage {
        timestamp: Some(200),
        group_v2: group(),
        poll_vote: Some(PollVote {
            target_author_aci_binary: Some(alice.clone()),
            target_sent_timestamp: Some(100),
            option_indexes: indexes,
            vote_count: Some(2),
        }),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(BOB, vote(vec![1, 1, 0])), &ctx()),
        vec![Action::PollVote {
            conversation: ConversationId::group(&group_key()),
            voter: Some(BOB.into()),
            target_author: Some(ALICE.into()),
            target_sent_at: 100,
            options: vec![1, 0],
            vote_count: 2,
            sent_at: 200,
        }]
    );
    assert!(map_content(&content(BOB, vote(vec![10])), &ctx()).is_empty(), "options are 0 to 9");
    // Ours from another device: the voter is us.
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            timestamp: Some(200),
            message: Some(vote(vec![])),
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    assert!(matches!(actions.as_slice(), [Action::PollVote { voter: None, options, .. }] if options.is_empty()));

    let end = DataMessage {
        timestamp: Some(300),
        group_v2: group(),
        poll_terminate: Some(PollTerminate { target_sent_timestamp: Some(100) }),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ALICE, end), &ctx()),
        vec![Action::PollEnd { conversation: ConversationId::group(&group_key()), by: Some(ALICE.into()), target_sent_at: 100, sent_at: 300 }]
    );
}

#[test]
fn who_may_add_members_follows_member_access() {
    use presage::libsignal_service::groups_v2::{AccessRequired, Role};
    use presage::libsignal_service::prelude::ProfileKey;
    use presage::libsignal_service::protocol::Aci;
    use presage::model::groups::{AccessControl, Group, Member};
    let member = |sid: &str, role| Member {
        aci: Aci::parse_from_service_id_string(sid).unwrap(),
        role,
        profile_key: ProfileKey::create([0; 32]),
        joined_at_revision: 0,
        label: None,
        label_emoji: None,
    };
    let group = |members_access| Group {
        title: "Hikers".into(),
        avatar: String::new(),
        disappearing_messages_timer: None,
        access_control: Some(AccessControl {
            attributes: AccessRequired::Member,
            members: members_access,
            add_from_invite_link: AccessRequired::Unsatisfiable,
            member_label: AccessRequired::Member,
        }),
        revision: 1,
        members: vec![member(ME, Role::Default), member(ALICE, Role::Administrator)],
        pending_members: vec![],
        requesting_members: vec![],
        invite_link_password: vec![],
        description: None,
    };
    assert!(can_add_group_members(&group(AccessRequired::Member), ME));
    assert!(!can_add_group_members(&group(AccessRequired::Administrator), ME), "admins only");
    assert!(can_add_group_members(&group(AccessRequired::Administrator), ALICE), "an admin always may");
    assert!(!can_add_group_members(&group(AccessRequired::Member), BOB), "not in the group");
}

fn view_once_photo(ts: u64) -> DataMessage {
    DataMessage {
        timestamp: Some(ts),
        is_view_once: Some(true),
        attachments: vec![AttachmentPointer {
            content_type: Some("image/jpeg".into()),
            size: Some(1000),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn view_once_messages_keep_their_one_photo_or_nothing() {
    use signal_store::ViewOnce;
    let actions = map_content(&content(ALICE, view_once_photo(100)), &ctx());
    let [Action::Insert { message, pointers, view_once, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((*view_once, pointers.len(), message.attachments.len()), (Some(ViewOnce::Ready), 1, 1));

    // Not one Desktop would show once (`isValidTapToView`): kept, but
    // nothing of it.
    let invalid = |dm: DataMessage| {
        let actions = map_content(&content(ALICE, dm), &ctx());
        let [Action::Insert { message, pointers, view_once, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(*view_once, Some(ViewOnce::Invalid));
        assert!(pointers.is_empty() && message.attachments.is_empty() && message.body.is_empty() && message.quote.is_none());
    };
    invalid(DataMessage { body: Some("look".into()), ..view_once_photo(101) });
    let mut two = view_once_photo(102);
    two.attachments.push(two.attachments[0].clone());
    invalid(two);
    let mut pdf = view_once_photo(103);
    pdf.attachments[0].content_type = Some("application/pdf".into());
    invalid(pdf);
    invalid(DataMessage {
        quote: Some(data_message::Quote { id: Some(5), ..Default::default() }),
        ..view_once_photo(104)
    });
    invalid(DataMessage { attachments: vec![], ..view_once_photo(105) });

    // Ours from another device: erased once sent, as Desktop does.
    let sync = SyncMessage {
        content: Some(sync_message::Content::Sent(sync_message::Sent {
            destination_service_id: Some(ALICE.into()),
            timestamp: Some(200),
            message: Some(view_once_photo(200)),
            ..Default::default()
        })),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync), &ctx());
    let [Action::Insert { message, pointers, view_once, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!((*view_once, message.direction), (Some(ViewOnce::Gone), Direction::Outgoing));
    assert!(pointers.is_empty() && message.attachments.is_empty());

    // An ordinary photo isn't view-once.
    let photo = DataMessage { is_view_once: None, ..view_once_photo(300) };
    let actions = map_content(&content(ALICE, photo), &ctx());
    assert!(matches!(actions.as_slice(), [Action::Insert { view_once: None, .. }]), "{actions:?}");
}

#[test]
fn view_once_open_syncs_name_the_message() {
    let open = |sender_aci: Option<String>, sender_aci_binary: Option<Vec<u8>>| SyncMessage {
        content: Some(sync_message::Content::ViewOnceOpen(sync_message::ViewOnceOpen {
            sender_aci,
            sender_aci_binary,
            timestamp: Some(100),
        })),
        ..Default::default()
    };
    let expected = vec![Action::ViewOnceOpen { author: ALICE.into(), sent_at: 100 }];
    assert_eq!(map_content(&content(ME, open(Some(ALICE.into()), None)), &ctx()), expected);
    let binary = uuid::Uuid::parse_str(ALICE).unwrap().as_bytes().to_vec();
    assert_eq!(map_content(&content(ME, open(None, Some(binary))), &ctx()), expected);
    assert!(map_content(&content(ME, open(None, None)), &ctx()).is_empty());
}

#[test]
fn message_request_answers_from_our_other_devices() {
    use signal_store::MessageRequestResponse;
    let alice = presage::libsignal_service::protocol::Aci::parse_from_service_id_string(ALICE).unwrap();
    let sync = |answer: sync_message::MessageRequestResponse| SyncMessage {
        content: Some(sync_message::Content::MessageRequestResponse(answer)),
        ..Default::default()
    };
    // A person by raw UUID (binaryServiceId) or by string.
    let binary = sync_message::MessageRequestResponse {
        thread_aci_binary: Some(uuid::Uuid::from(alice).into_bytes().to_vec()),
        r#type: Some(sync_message::message_request_response::Type::Accept as i32),
        ..Default::default()
    };
    assert_eq!(
        map_content(&content(ME, sync(binary)), &ctx()),
        vec![Action::MessageRequestResponse {
            thread: ThreadRef::Direct(ALICE.into()),
            response: MessageRequestResponse::Accept,
            at: 500
        }]
    );
    let string = sync_message::MessageRequestResponse {
        thread_aci: Some(ALICE.into()),
        r#type: Some(sync_message::message_request_response::Type::BlockAndSpam as i32),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync(string)), &ctx());
    assert!(matches!(&actions[..], [Action::MessageRequestResponse { response: MessageRequestResponse::BlockAndSpam, .. }]));
    // A group by its 32-byte id; another length names nothing.
    let group = sync_message::MessageRequestResponse {
        group_id: Some(vec![4; 32]),
        thread_aci: Some(ALICE.into()),
        r#type: Some(sync_message::message_request_response::Type::Delete as i32),
        ..Default::default()
    };
    let actions = map_content(&content(ME, sync(group)), &ctx());
    assert!(matches!(&actions[..], [Action::MessageRequestResponse { thread: ThreadRef::GroupId(id), .. }] if id == &vec![4; 32]));
    let bad = sync_message::MessageRequestResponse { group_id: Some(vec![4; 16]), thread_aci: Some(ALICE.into()), ..Default::default() };
    assert!(map_content(&content(ME, sync(bad)), &ctx()).is_empty());
}

#[test]
fn message_request_syncs_we_send_name_the_chat_as_desktop() {
    use signal_store::MessageRequestResponse;
    let alice = presage::libsignal_service::protocol::Aci::parse_from_service_id_string(ALICE).unwrap();
    let binary = message_request_sync(Some(ALICE), None, MessageRequestResponse::Accept, true);
    assert_eq!(binary.thread_aci_binary, Some(uuid::Uuid::from(alice).into_bytes().to_vec()));
    assert_eq!((binary.thread_aci.as_deref(), binary.r#type), (None, Some(1)));
    let string = message_request_sync(Some(ALICE), None, MessageRequestResponse::Spam, false);
    assert_eq!((string.thread_aci.as_deref(), string.thread_aci_binary.as_deref(), string.r#type), (Some(ALICE), None, Some(5)));
    let group = message_request_sync(None, Some(vec![7; 32]), MessageRequestResponse::Block, true);
    assert_eq!((group.group_id, group.thread_aci, group.thread_aci_binary), (Some(vec![7; 32]), None, None));
    // What we send reads back as the same chat and answer.
    assert_eq!(message_request_thread(&binary), Some(ThreadRef::Direct(ALICE.into())));
    assert_eq!(message_request_thread(&string), Some(ThreadRef::Direct(ALICE.into())));
}

#[test]
fn who_added_us_outright_is_kept_for_message_requests() {
    use presage::libsignal_service::groups_v2::{GroupChange, GroupChanges, Member, Role};
    use presage::libsignal_service::protocol::Aci;
    let alice = Aci::parse_from_service_id_string(ALICE).unwrap();
    let me = Aci::parse_from_service_id_string(ME).unwrap();
    let key = presage::libsignal_service::zkgroup::profiles::ProfileKey::create([1; 32]);
    let added = |editor, aci| GroupChanges {
        group_id: [0; 32],
        editor,
        version: 4,
        changes: vec![GroupChange::NewMember(Member { aci, role: Role::Default, profile_key: key, joined_at_version: 4, label: None, label_emoji: None })],
        change_epoch: 0,
    };
    // Desktop's `addedBy`: whoever added us.
    assert_eq!(summarize_group_change(&added(alice, me), ME, None, &Default::default()).added_by.as_deref(), Some(ALICE));
    // Not when we added ourselves (by the link), nor for someone else.
    assert_eq!(summarize_group_change(&added(me, me), ME, None, &Default::default()).added_by, None);
    let bob = Aci::parse_from_service_id_string(BOB).unwrap();
    assert_eq!(summarize_group_change(&added(alice, bob), ME, None, &Default::default()).added_by, None);
}
