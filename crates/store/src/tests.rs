use super::*;

/// A database on disk, opened as the app opens it: reads on their own
/// connections, which can't write, and writes queued for one. (The
/// directory is left for the OS to clear.)
async fn test_db() -> AppDb {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = AppDb::open(&dir.join("app.db"), &[3; 32]).await.unwrap();
    // Never closed, as the app keeps its pools: sqlx closing connections on
    // its threads at exit races SQLCipher's exit-time shutdown, which
    // crashed the test process after every test had passed.
    std::mem::forget(db.clone());
    db
}

#[tokio::test]
async fn settings_are_removed_by_key_and_by_prefix() {
    let db = test_db().await;
    for key in ["kt.account.a", "kt.account.b", "kt.accounts", "kt.account", "kt.distinguished"] {
        db.set_setting(key, "1").await.unwrap();
    }
    // Only keys under the prefix go, not their neighbours in key order.
    assert_eq!(db.remove_settings_with_prefix("kt.account.").await.unwrap(), 2);
    assert_eq!(db.setting("kt.account.a").await.unwrap(), None);
    assert_eq!(db.setting("kt.accounts").await.unwrap().as_deref(), Some("1"));
    assert_eq!(db.setting("kt.account").await.unwrap().as_deref(), Some("1"));
    db.remove_setting("kt.distinguished").await.unwrap();
    assert_eq!(db.setting("kt.distinguished").await.unwrap(), None);
}

fn conv(s: &str) -> ConversationId {
    ConversationId(s.to_owned())
}

fn incoming(c: &str, author: &str, sent_at: i64, body: &str) -> NewMessage {
    NewMessage {
        conversation_id: conv(c),
        sent_at,
        received_at: sent_at + 5,
        direction: Direction::Incoming,
        author: Some(author.to_owned()),
        author_name: Some(format!("{author} Name")),
        body: body.to_owned(),
        status: DeliveryStatus::Delivered,
        quote: None,
        attachments: vec![],
        link_preview: None,
        expires_in: 0,
    }
}

fn outgoing(c: &str, sent_at: i64, body: &str) -> NewMessage {
    NewMessage {
        direction: Direction::Outgoing,
        author: None,
        author_name: None,
        status: DeliveryStatus::Sending,
        ..incoming(c, "x", sent_at, body)
    }
}

#[tokio::test]
async fn conversation_upsert_and_ordering() {
    let db = test_db().await;
    db.upsert_conversation(
        &conv("d:alice"),
        ConversationUpdate {
            title: Some("Alice".into()),
            phone: Some("+15550001".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.upsert_conversation(&conv("g:00ff"), ConversationUpdate::default()).await.unwrap();

    let a = db.conversation(&conv("d:alice")).await.unwrap().unwrap();
    assert_eq!(a.title, "Alice");
    assert_eq!(a.kind, ConversationKind::Direct);
    assert!(a.color < AVATAR_COLORS);
    let g = db.conversation(&conv("g:00ff")).await.unwrap().unwrap();
    assert_eq!(g.kind, ConversationKind::Group);

    // None fields keep stored values.
    db.upsert_conversation(&conv("d:alice"), ConversationUpdate { expire_timer: Some(60), ..Default::default() })
        .await
        .unwrap();
    let a = db.conversation(&conv("d:alice")).await.unwrap().unwrap();
    assert_eq!((a.title.as_str(), a.phone.as_deref(), a.expire_timer), ("Alice", Some("+15550001"), 60));

    db.insert_message(&incoming("g:00ff", "bob", 100, "hi")).await.unwrap();
    db.insert_message(&incoming("d:alice", "alice", 50, "older")).await.unwrap();
    let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id.0).collect();
    assert_eq!(ids, ["g:00ff", "d:alice"]);

    db.set_pinned(&conv("d:alice"), true).await.unwrap();
    let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id.0).collect();
    assert_eq!(ids, ["d:alice", "g:00ff"]);

    db.set_archived(&conv("g:00ff"), true).await.unwrap();
    assert_eq!(db.conversations().await.unwrap().len(), 1);
    assert_eq!(db.archived_conversations().await.unwrap().len(), 1);

    db.set_draft(&conv("d:alice"), Some("draft")).await.unwrap();
    assert_eq!(db.conversation(&conv("d:alice")).await.unwrap().unwrap().draft.as_deref(), Some("draft"));
    db.set_draft(&conv("d:alice"), Some("")).await.unwrap();
    assert_eq!(db.conversation(&conv("d:alice")).await.unwrap().unwrap().draft, None);

    // A formatted draft keeps its spans; a plain one, or none, drops them.
    let bold = [StyleSpan { start: 0, len: 4, style: TextStyle::Bold }];
    db.set_draft_with_styles(&conv("d:alice"), Some("bold move"), &bold).await.unwrap();
    let chat = db.conversation(&conv("d:alice")).await.unwrap().unwrap();
    assert_eq!((chat.draft.as_deref(), chat.draft_styles.as_slice()), (Some("bold move"), &bold[..]));
    assert!(db.conversation(&conv("g:00ff")).await.unwrap().unwrap().draft_styles.is_empty());
    db.set_draft(&conv("d:alice"), Some("plain now")).await.unwrap();
    assert!(db.conversation(&conv("d:alice")).await.unwrap().unwrap().draft_styles.is_empty());
    db.set_draft_with_styles(&conv("d:alice"), None, &bold).await.unwrap();
    assert_eq!(db.setting(&format!("{DRAFT_STYLES}d:alice")).await.unwrap(), None);
    db.set_draft_with_styles(&conv("d:alice"), Some("bold"), &bold).await.unwrap();
    db.remove_conversation(&conv("d:alice")).await.unwrap();
    assert_eq!(db.setting(&format!("{DRAFT_STYLES}d:alice")).await.unwrap(), None, "deleting the chat drops them");
}

#[tokio::test]
async fn groups_keep_who_may_add_and_whether_they_ended() {
    let db = test_db().await;
    let group = conv("g:0a");
    db.upsert_conversation(&group, ConversationUpdate { can_add_members: Some(true), ..Default::default() }).await.unwrap();
    let stored = db.conversation(&group).await.unwrap().unwrap();
    assert!(stored.can_add_members);
    assert!(!stored.ended, "a group hasn't ended until an admin ends it");
    // Unset fields keep their values.
    db.upsert_conversation(&group, ConversationUpdate { ended: Some(true), ..Default::default() }).await.unwrap();
    let stored = db.conversation(&group).await.unwrap().unwrap();
    assert!(stored.can_add_members && stored.ended);
}

#[tokio::test]
async fn groups_keep_their_administration_state() {
    let db = test_db().await;
    let group = conv("g:0b");
    db.upsert_conversation(&group, ConversationUpdate { title: Some("Hikers".into()), ..Default::default() }).await.unwrap();
    assert_eq!(db.conversation(&group).await.unwrap().unwrap().group_state, GroupState::default(), "none stored yet");
    let state = GroupState {
        announcements_only: true,
        info_admins_only: true,
        members_admins_only: false,
        labels_admins_only: true,
        link: Some(GroupLink { url: "https://signal.group/#abc".into(), approval: true }),
        invites: vec![GroupInvite { service_id: "PNI:p".into(), by: "a".into() }],
        requests: vec![GroupRequest { aci: "r".into(), at: 5 }],
        banned: vec!["b".into()],
        membership: Membership::Requesting { password: "cGFzcw==".into() },
    };
    db.upsert_conversation(&group, ConversationUpdate { group_state: Some(state.clone()), ..Default::default() }).await.unwrap();
    assert_eq!(db.conversation(&group).await.unwrap().unwrap().group_state, state);
    // Other updates leave it alone.
    db.upsert_conversation(&group, ConversationUpdate { title: Some("Hikers!".into()), ..Default::default() }).await.unwrap();
    assert_eq!(db.conversation(&group).await.unwrap().unwrap().group_state, state);
    // State from before a field existed still reads.
    let old: GroupState = serde_json::from_str(r#"{"announcements_only":true}"#).unwrap();
    assert!(old.announcements_only && old.membership == Membership::Member && old.link.is_none());
}

#[tokio::test]
async fn insert_dedupes_and_tracks_unread_and_preview() {
    let db = test_db().await;
    let id1 = db.insert_message(&incoming("g:aa", "bob", 10, "first")).await.unwrap();
    let dup = db.insert_message(&incoming("g:aa", "bob", 10, "first again")).await.unwrap();
    assert_eq!(id1, dup);
    // Same timestamp, different author: distinct message.
    let id2 = db.insert_message(&incoming("g:aa", "carol", 10, "x")).await.unwrap();
    assert_ne!(id1, id2);
    let id3 = db.insert_message(&outgoing("g:aa", 20, "mine")).await.unwrap();

    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!(c.unread_count, 2);
    assert_eq!(c.last_message_at, 20);
    assert_eq!(c.last_message_preview.as_deref(), Some("You: mine"));

    // An older message arriving late doesn't take over the preview.
    db.insert_message(&incoming("g:aa", "bob", 5, "late")).await.unwrap();
    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("You: mine"));
    assert_eq!(c.unread_count, 3);

    let unread = db.mark_read(&conv("g:aa")).await.unwrap();
    assert_eq!(unread, vec![("bob".into(), 5), ("bob".into(), 10), ("carol".into(), 10)]);
    assert_eq!(db.conversation(&conv("g:aa")).await.unwrap().unwrap().unread_count, 0);
    assert!(db.mark_read(&conv("g:aa")).await.unwrap().is_empty());

    let m = db.find_message(&conv("g:aa"), None, 20).await.unwrap().unwrap();
    assert_eq!(m.id, id3);
    assert_eq!(m.direction, Direction::Outgoing);
    assert!(db.find_message(&conv("g:aa"), Some("bob"), 20).await.unwrap().is_none());
}

#[tokio::test]
async fn group_preview_has_author_prefix() {
    let db = test_db().await;
    db.insert_message(&incoming("g:aa", "bob", 10, "hello\nsecond line")).await.unwrap();
    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("bob: hello second line"));
    db.insert_message(&incoming("d:bob", "bob", 10, "hello")).await.unwrap();
    let c = db.conversation(&conv("d:bob")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("hello"));
}

#[tokio::test]
async fn pagination_is_oldest_first() {
    let db = test_db().await;
    for i in 0..10 {
        db.insert_message(&incoming("d:a", "a", 100 + i, &format!("m{i}"))).await.unwrap();
    }
    let page = db.messages(&conv("d:a"), None, 4).await.unwrap();
    let bodies: Vec<_> = page.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["m6", "m7", "m8", "m9"]);
    let page = db.messages(&conv("d:a"), Some(page[0].id), 4).await.unwrap();
    let bodies: Vec<_> = page.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["m2", "m3", "m4", "m5"]);
    let page = db.messages(&conv("d:a"), Some(page[0].id), 4).await.unwrap();
    assert_eq!(page.len(), 2);
    assert!(db.messages(&conv("d:other"), None, 4).await.unwrap().is_empty());
}

#[tokio::test]
async fn status_only_moves_forward() {
    let db = test_db().await;
    let id = db.insert_message(&outgoing("d:a", 1, "x")).await.unwrap();
    let status = |db: AppDb| async move { db.message(id).await.unwrap().unwrap().status };

    db.update_status(&conv("d:a"), 1, DeliveryStatus::Delivered).await.unwrap();
    assert_eq!(status(db.clone()).await, DeliveryStatus::Delivered);
    db.update_status(&conv("d:a"), 1, DeliveryStatus::Sent).await.unwrap();
    assert_eq!(status(db.clone()).await, DeliveryStatus::Delivered);
    db.update_status(&conv("d:a"), 1, DeliveryStatus::Failed).await.unwrap();
    assert_eq!(status(db.clone()).await, DeliveryStatus::Delivered);
    let changed = db.update_status_by_sent_at(1, DeliveryStatus::Read).await.unwrap();
    assert_eq!(changed, vec![(conv("d:a"), id)]);
    assert_eq!(status(db.clone()).await, DeliveryStatus::Read);
    assert!(db.update_status_by_sent_at(1, DeliveryStatus::Read).await.unwrap().is_empty());

    let id2 = db.insert_message(&outgoing("d:a", 2, "y")).await.unwrap();
    db.update_status(&conv("d:a"), 2, DeliveryStatus::Failed).await.unwrap();
    assert_eq!(db.message(id2).await.unwrap().unwrap().status, DeliveryStatus::Failed);
    db.update_status(&conv("d:a"), 2, DeliveryStatus::Delivered).await.unwrap();
    assert_eq!(db.message(id2).await.unwrap().unwrap().status, DeliveryStatus::Failed);
}

#[tokio::test]
async fn reactions_replace_per_author() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("d:a", "a", 1, "x")).await.unwrap();
    let r = |emoji: &str, author: Option<&str>| Reaction {
        emoji: emoji.into(),
        author: author.map(Into::into),
        sent_at: 5,
    };
    db.set_reaction(id, r("👍", None)).await.unwrap();
    db.set_reaction(id, r("❤️", Some("a"))).await.unwrap();
    db.set_reaction(id, r("😂", None)).await.unwrap();
    let m = db.message(id).await.unwrap().unwrap();
    let emojis: Vec<_> = m.reactions.iter().map(|r| r.emoji.as_str()).collect();
    assert_eq!(emojis, ["❤️", "😂"]);
    db.remove_reaction(id, Some("a")).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().reactions.len(), 1);
    // Unknown message: no-op.
    db.set_reaction(9999, r("👍", None)).await.unwrap();
}

#[tokio::test]
async fn delete_edit_attachments_and_search() {
    let db = test_db().await;
    let mut m = incoming("d:a", "a", 1, "the quick brown fox");
    m.attachments = vec![AttachmentMeta {
        content_type: "image/png".into(),
        file_name: Some("a.png".into()),
        size: 10,
        width: Some(1),
        height: Some(1),
        local_path: None,
        sticker: false,
        gif: false,
        voice: false,
        remote: None,
        thumbnail_path: None,
    }];
    m.quote = Some(Quote { author: None, author_name: "You".into(), sent_at: 0, text: "q".into(), thumbnail: None, attachment: None, story: false });
    let id = db.insert_message(&m).await.unwrap();
    let id2 = db.insert_message(&incoming("d:b", "b", 2, "Crème brûlée foxes")).await.unwrap();

    let got = db.message(id).await.unwrap().unwrap();
    assert_eq!(got.quote.unwrap().text, "q");
    // A backup thumbnail first, then the file; each kept.
    assert!(db.set_attachment_thumbnail(id, 0, "/tmp/a-thumbnail.webp").await.unwrap());
    assert!(!db.set_attachment_thumbnail(id, 5, "/tmp/nope").await.unwrap(), "no such attachment");
    assert!(db.set_attachment_path(id, 0, "/tmp/a.png").await.unwrap());
    assert!(!db.set_attachment_path(id, 5, "/tmp/nope").await.unwrap(), "no such attachment");
    let attachment = db.message(id).await.unwrap().unwrap().attachments[0].clone();
    assert_eq!(attachment.local_path.as_deref(), Some("/tmp/a.png"));
    assert_eq!(attachment.thumbnail_path.as_deref(), Some("/tmp/a-thumbnail.webp"));

    let hits = db.search_messages("fox", 10).await.unwrap();
    assert_eq!(hits.iter().map(|h| h.message_id).collect::<Vec<_>>(), [id2, id]);
    assert!(hits[1].snippet.contains("\u{2}fox\u{3}"), "{}", hits[1].snippet);
    // Diacritics folded; FTS syntax in user input is inert.
    assert_eq!(db.search_messages("creme", 10).await.unwrap().len(), 1);
    assert!(db.search_messages("\"fox OR NEAR(", 10).await.is_ok());
    assert!(db.search_messages("  ", 10).await.unwrap().is_empty());

    db.edit_message(id, "slow green turtle", 50).await.unwrap();
    let got = db.message(id).await.unwrap().unwrap();
    assert!(got.edited);
    assert_eq!(db.search_messages("fox", 10).await.unwrap().len(), 1);
    assert_eq!(db.search_messages("turtle", 10).await.unwrap().len(), 1);
    assert_eq!(
        db.conversation(&conv("d:a")).await.unwrap().unwrap().last_message_preview.as_deref(),
        Some("slow green turtle")
    );

    db.mark_deleted(id).await.unwrap();
    let got = db.message(id).await.unwrap().unwrap();
    assert!(got.deleted && got.body.is_empty() && got.attachments.is_empty());
    assert!(db.search_messages("turtle", 10).await.unwrap().is_empty());
    assert_eq!(
        db.conversation(&conv("d:a")).await.unwrap().unwrap().last_message_preview.as_deref(),
        Some("This message was deleted")
    );
    // Edits after delete are ignored.
    db.edit_message(id, "resurrected", 60).await.unwrap();
    assert!(db.message(id).await.unwrap().unwrap().body.is_empty());
    let recorded: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_edits WHERE sent_at = 60").fetch_one(&db.pool).await.unwrap();
    assert_eq!(recorded, 0, "an edit that didn't apply doesn't name the message");
}

#[tokio::test]
async fn the_next_audio_is_the_chats_next_message_with_audio() {
    let db = test_db().await;
    let with = |mut m: NewMessage, content_type: &str| {
        m.attachments = vec![AttachmentMeta {
            content_type: content_type.into(),
            file_name: None,
            size: 10,
            width: None,
            height: None,
            local_path: Some("/x".into()),
            sticker: false,
            gif: false,
            voice: content_type == "audio/aac",
            remote: None,
            thumbnail_path: None,
        }];
        m
    };
    let first = db.insert_message(&with(incoming("d:a", "a", 10, ""), "audio/aac")).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 20, "words")).await.unwrap();
    db.insert_message(&with(incoming("d:a", "a", 25, ""), "image/png")).await.unwrap();
    // Another chat's audio in between doesn't count.
    db.insert_message(&with(incoming("d:b", "b", 27, ""), "audio/aac")).await.unwrap();
    let song = db.insert_message(&with(outgoing("d:a", 30, ""), "audio/mpeg")).await.unwrap();
    let last = db.insert_message(&with(incoming("d:a", "a", 40, ""), "audio/aac")).await.unwrap();
    assert_eq!(db.next_audio(first).await.unwrap().map(|m| m.id), Some(song), "voice messages and audio files alike");
    assert_eq!(db.next_audio(song).await.unwrap().map(|m| m.id), Some(last));
    db.mark_deleted(last).await.unwrap();
    assert_eq!(db.next_audio(song).await.unwrap(), None, "deleted ones are skipped");
}

#[tokio::test]
async fn the_first_unread_message_and_unread_mentions_of_us() {
    let db = test_db().await;
    let chat = conv("g:x");
    db.insert_message(&incoming("g:x", "a", 10, "old")).await.unwrap();
    db.mark_read(&chat).await.unwrap();
    assert_eq!(db.first_unread(&chat).await.unwrap(), None);
    let first = db.insert_message(&incoming("g:x", "a", 20, "@Me look")).await.unwrap();
    db.set_mentions(first, &[MentionSpan { start: 0, len: 3, aci: "me".into() }]).await.unwrap();
    let other = db.insert_message(&incoming("g:x", "b", 30, "@Bo hi")).await.unwrap();
    db.set_mentions(other, &[MentionSpan { start: 0, len: 3, aci: "bo".into() }]).await.unwrap();
    let again = db.insert_message(&incoming("g:x", "b", 40, "and @Me")).await.unwrap();
    db.set_mentions(again, &[MentionSpan { start: 4, len: 3, aci: "me".into() }]).await.unwrap();
    assert_eq!(db.first_unread(&chat).await.unwrap(), Some((first, 3)));
    assert_eq!(db.unread_mentions(&chat, "me").await.unwrap(), [first, again]);
    db.mark_read(&chat).await.unwrap();
    assert!(db.unread_mentions(&chat, "me").await.unwrap().is_empty());
}

#[tokio::test]
async fn edits_keep_the_versions_they_replace() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("g:x", "a", 10, "@Bo meet at 8")).await.unwrap();
    db.set_mentions(id, &[MentionSpan { start: 0, len: 3, aci: "b".into() }]).await.unwrap();
    db.set_styles(id, &[StyleSpan { start: 12, len: 1, style: TextStyle::Bold }]).await.unwrap();
    assert!(db.edit_history(id).await.unwrap().is_empty(), "never edited");

    db.edit_message(id, "meet at 9", 20).await.unwrap();
    db.set_mentions(id, &[]).await.unwrap();
    db.edit_message(id, "meet at 9:30", 30).await.unwrap();
    // A repeat of the last edit (a re-delivery) adds nothing.
    db.edit_message(id, "meet at 9:30", 30).await.unwrap();
    let history = db.edit_history(id).await.unwrap();
    assert_eq!(
        history.iter().map(|r| (r.sent_at, r.body.as_str())).collect::<Vec<_>>(),
        [(10, "@Bo meet at 8"), (20, "meet at 9")],
        "oldest first, each dated by the timestamp that named it"
    );
    assert_eq!(history[0].mentions, [MentionSpan { start: 0, len: 3, aci: "b".into() }]);
    assert_eq!(history[0].styles, [StyleSpan { start: 12, len: 1, style: TextStyle::Bold }]);
    assert!(history[1].mentions.is_empty() && history[1].styles.is_empty());

    // A backup's own revisions replace it.
    let revision = EditRevision { sent_at: 5, body: "first".into(), mentions: vec![], styles: vec![] };
    db.set_edit_history(id, std::slice::from_ref(&revision)).await.unwrap();
    assert_eq!(db.edit_history(id).await.unwrap(), [revision]);

    // Deleting it for everyone forgets the earlier versions too.
    db.mark_deleted(id).await.unwrap();
    assert!(db.edit_history(id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_deletes_progress_is_kept_on_the_deleted_message() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("g:x", "a", 10, "oops")).await.unwrap();
    assert!(!db.set_delete_status(id, DeliveryStatus::Failed).await.unwrap(), "not deleted yet");
    db.mark_deleted(id).await.unwrap();
    assert!(db.set_delete_status(id, DeliveryStatus::Failed).await.unwrap());
    assert_eq!(db.message(id).await.unwrap().unwrap().status, DeliveryStatus::Failed);
    // A retry starts it again.
    db.set_delete_status(id, DeliveryStatus::Sending).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().status, DeliveryStatus::Sending);

    // Ours: the message's recipients become the delete's, and receipts for
    // the message don't move them.
    let ours = db.insert_message(&outgoing("g:x", 20, "oops")).await.unwrap();
    db.record_send_states(ours, &[("b".into(), SendStatus::Sent, None)], 21).await.unwrap();
    db.mark_deleted(ours).await.unwrap();
    assert!(db.start_delete_send(ours).await.unwrap());
    assert!(db.send_states(ours).await.unwrap().is_empty());
    let summary = db
        .record_send_states(ours, &[("b".into(), SendStatus::Sent, None), ("c".into(), SendStatus::Failed, Some("x".into()))], 22)
        .await
        .unwrap();
    assert_eq!(summary, Some(DeliveryStatus::PartialSent));
    assert_eq!(db.failed_recipients(ours).await.unwrap(), ["c"]);
    assert!(db.apply_receipt_from("c", 20, SendStatus::Delivered, 23).await.unwrap().is_empty());
    assert_eq!(db.failed_recipients(ours).await.unwrap(), ["c"]);
}

#[tokio::test]
async fn admin_deletes_note_the_admin_once() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("g:x", "a", 10, "off topic")).await.unwrap();
    let mut reacted = db.message(id).await.unwrap().unwrap();
    assert!(reacted.deleted_by_admin.is_none());
    db.set_reaction(id, Reaction { emoji: "👍".into(), author: Some("b".into()), sent_at: 11 }).await.unwrap();

    let admin = DeletedByAdmin { by: Some("b".into()) };
    assert!(db.mark_deleted_by_admin(id, &admin).await.unwrap());
    reacted = db.message(id).await.unwrap().unwrap();
    assert!(reacted.deleted && reacted.body.is_empty() && reacted.reactions.is_empty());
    assert_eq!(reacted.deleted_by_admin, Some(admin.clone()));
    let preview = db.conversation(&conv("g:x")).await.unwrap().unwrap().last_message_preview.unwrap();
    assert!(preview.ends_with("This message was deleted"), "{preview}");

    // A second delete, ours this time, changes nothing.
    assert!(!db.mark_deleted_by_admin(id, &DeletedByAdmin { by: None }).await.unwrap());
    assert_eq!(db.message(id).await.unwrap().unwrap().deleted_by_admin, Some(admin));

    // The author's own delete leaves it unset.
    let own = db.insert_message(&incoming("g:x", "a", 20, "mine")).await.unwrap();
    db.mark_deleted(own).await.unwrap();
    assert!(db.message(own).await.unwrap().unwrap().deleted_by_admin.is_none());
}

#[tokio::test]
async fn attachment_only_preview() {
    let db = test_db().await;
    let mut m = incoming("d:a", "a", 1, "");
    m.attachments = vec![AttachmentMeta {
        content_type: "image/jpeg".into(),
        file_name: None,
        size: 1,
        width: None,
        height: None,
        local_path: None,
        sticker: false,
        gif: false,
        voice: false,
        remote: None,
        thumbnail_path: None,
    }];
    db.insert_message(&m).await.unwrap();
    assert_eq!(
        db.conversation(&conv("d:a")).await.unwrap().unwrap().last_message_preview.as_deref(),
        Some("📷 Photo")
    );
}

#[tokio::test]
async fn emoji_usage_and_settings() {
    let db = test_db().await;
    db.record_emoji_use("👍").await.unwrap();
    db.record_emoji_use("👍").await.unwrap();
    db.record_emoji_use("🎉").await.unwrap();
    let usage = db.emoji_usage().await.unwrap();
    let thumbs = usage.iter().find(|u| u.emoji == "👍").unwrap();
    assert_eq!(thumbs.count, 2);
    assert_eq!(usage.len(), 2);

    assert_eq!(db.setting("theme").await.unwrap(), None);
    db.set_setting("theme", "dark").await.unwrap();
    db.set_setting("theme", "light").await.unwrap();
    assert_eq!(db.setting("theme").await.unwrap().as_deref(), Some("light"));
}

#[tokio::test]
async fn conversation_id_helpers() {
    let g = ConversationId::group(&[0x00, 0xab, 0xff]);
    assert_eq!(g.0, "g:00abff");
    assert_eq!(g.as_group(), Some(vec![0x00, 0xab, 0xff]));
    assert!(g.is_group());
    let d = ConversationId::direct("abc");
    assert_eq!(d.as_direct(), Some("abc"));
    assert_eq!(d.as_group(), None);
    assert_eq!(ConversationId("g:zz".into()).as_group(), None);
    assert_eq!(ConversationId("g:abc".into()).as_group(), None);
}

#[tokio::test]
async fn encrypted_file_rejects_wrong_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let key = [7u8; 32];
    {
        let db = AppDb::open(&path, &key).await.unwrap();
        db.insert_message(&incoming("d:a", "a", 1, "secret plaintext")).await.unwrap();
        db.close().await;
    }
    // Reopens with the right key.
    {
        let db = AppDb::open(&path, &key).await.unwrap();
        assert_eq!(db.conversations().await.unwrap().len(), 1);
        db.close().await;
    }
    // Wrong key cannot read it.
    assert!(AppDb::open(&path, &[8u8; 32]).await.is_err());
    // The file holds no plaintext.
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.windows(6).any(|w| w == b"secret"));
    assert!(!bytes.starts_with(b"SQLite format 3"));
}

#[tokio::test]
async fn read_sync_marks_single_message() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "one")).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 2, "two")).await.unwrap();
    assert_eq!(db.mark_message_read("a", 1).await.unwrap(), Some(conv("d:a")));
    assert_eq!(db.mark_message_read("a", 1).await.unwrap(), None);
    assert_eq!(db.mark_message_read("zz", 1).await.unwrap(), None);
    assert_eq!(db.conversation(&conv("d:a")).await.unwrap().unwrap().unread_count, 1);
    assert_eq!(db.mark_read(&conv("d:a")).await.unwrap(), vec![("a".into(), 2)]);
}

#[tokio::test]
async fn last_outgoing_status_follows_latest_message() {
    let db = test_db().await;
    db.upsert_conversation(&conv("d:jon"), ConversationUpdate::default()).await.unwrap();
    let status = |db: AppDb| async move { db.conversation(&conv("d:jon")).await.unwrap().unwrap().last_outgoing_status };
    assert_eq!(status(db.clone()).await, None);

    db.insert_message(&outgoing("d:jon", 1_000, "I did not")).await.unwrap();
    assert_eq!(status(db.clone()).await, Some(DeliveryStatus::Sending));
    db.update_status(&conv("d:jon"), 1_000, DeliveryStatus::Read).await.unwrap();
    assert_eq!(status(db.clone()).await, Some(DeliveryStatus::Read));

    // A newer incoming message hides the receipt.
    db.insert_message(&incoming("d:jon", "jon", 2_000, "ok")).await.unwrap();
    assert_eq!(status(db.clone()).await, None);

    db.set_avatar(&conv("d:jon"), Some("/tmp/a.jpg")).await.unwrap();
    let c = db.conversation(&conv("d:jon")).await.unwrap().unwrap();
    assert_eq!(c.avatar_path.as_deref(), Some("/tmp/a.jpg"));
}

#[tokio::test]
async fn contact_names_are_replaced_wholesale() {
    let db = test_db().await;
    db.replace_contact_names(&[("a".into(), "Jon".into()), ("b".into(), "Bo".into())]).await.unwrap();
    assert_eq!(db.contact_name("a").await.unwrap().as_deref(), Some("Jon"));
    db.replace_contact_names(&[("a".into(), "Jonathan".into())]).await.unwrap();
    assert_eq!(db.contact_name("a").await.unwrap().as_deref(), Some("Jonathan"));
    assert_eq!(db.contact_name("b").await.unwrap(), None);
}

#[tokio::test]
async fn nicknames_follow_storage_and_local_edits() {
    let db = test_db().await;
    let nick = |given: &str, family: &str, note: &str, plain: Option<&str>| ContactNickname {
        given: given.into(),
        family: family.into(),
        note: note.into(),
        plain_name: plain.map(Into::into),
    };
    // From a sync: a nickname over the profile name, and a note alone.
    db.replace_contacts(
        &[("a".into(), "Mo".into()), ("b".into(), "Bo Lee".into())],
        &[("a".into(), nick("Mo", "", "Met at the gym", Some("Maurice Diaz"))), ("b".into(), nick("", "", "", None))],
    )
    .await
    .unwrap();
    assert_eq!(db.contact_nickname("a").await.unwrap(), Some(nick("Mo", "", "Met at the gym", Some("Maurice Diaz"))));
    assert_eq!(db.contact_nickname("b").await.unwrap(), None, "empty ones aren't kept");

    // Edited here: the display name follows, and the plain name is kept.
    let name = db.set_contact_nickname("a", &nick(" Moe ", "D", "", None)).await.unwrap();
    assert_eq!(name.as_deref(), Some("Moe D"));
    assert_eq!(db.contact_name("a").await.unwrap().as_deref(), Some("Moe D"));
    assert_eq!(db.contact_nickname("a").await.unwrap(), Some(nick("Moe", "D", "", Some("Maurice Diaz"))));
    // Deleted: back to the plain name, and no row.
    assert_eq!(db.set_contact_nickname("a", &ContactNickname::default()).await.unwrap().as_deref(), Some("Maurice Diaz"));
    assert_eq!(db.contact_name("a").await.unwrap().as_deref(), Some("Maurice Diaz"));
    assert_eq!(db.contact_nickname("a").await.unwrap(), None);

    // A first nickname remembers the name they had.
    db.set_contact_nickname("b", &nick("Bobo", "", "", None)).await.unwrap();
    assert_eq!(db.contact_nickname("b").await.unwrap().unwrap().plain_name.as_deref(), Some("Bo Lee"));
    assert_eq!(db.display_names(&["b".into()]).await.unwrap(), [("b".to_owned(), "Bobo".to_owned())]);
    // Someone with no saved name: none after, so their profile name shows.
    db.set_contact_nickname("c", &nick("", "", "Just a note", None)).await.unwrap();
    assert_eq!(db.contact_name("c").await.unwrap(), None);
    assert_eq!(db.set_contact_nickname("c", &ContactNickname::default()).await.unwrap(), None);

    // The next sync replaces them all.
    db.replace_contact_names(&[]).await.unwrap();
    assert_eq!(db.contact_nickname("b").await.unwrap(), None);
}

#[tokio::test]
async fn rename_authors_updates_messages_and_quotes() {
    let db = test_db().await;
    let mut first = incoming("g:01", "jon", 1, "hi");
    first.author_name = None;
    let first = db.insert_message(&first).await.unwrap();
    let mut reply = incoming("g:01", "amy", 2, "re");
    reply.quote = Some(Quote { author: Some("jon".into()), author_name: "jon".into(), sent_at: 1, text: "hi".into(), thumbnail: None, attachment: None, story: false });
    let reply = db.insert_message(&reply).await.unwrap();

    let mut authors = db.message_authors().await.unwrap();
    authors.sort();
    assert_eq!(authors, ["amy", "jon"]);

    assert_eq!(db.rename_authors(&[("jon".into(), "Jon".into())]).await.unwrap(), 2);
    assert_eq!(db.message(first).await.unwrap().unwrap().author_name.as_deref(), Some("Jon"));
    assert_eq!(db.message(reply).await.unwrap().unwrap().quote.unwrap().author_name, "Jon");
    // Already up to date: nothing changes.
    assert_eq!(db.rename_authors(&[("jon".into(), "Jon".into())]).await.unwrap(), 0);
}

#[tokio::test]
async fn pinned_conversations_follow_the_account_order() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "old")).await.unwrap();
    db.insert_message(&incoming("d:b", "b", 2, "newer")).await.unwrap();
    db.insert_message(&incoming("d:c", "c", 3, "newest")).await.unwrap();
    let order = |db: AppDb| async move {
        db.conversations().await.unwrap().into_iter().map(|c| (c.id.0, c.pinned)).collect::<Vec<_>>()
    };

    // "d:empty" has no messages yet but is pinned, so it is listed.
    assert!(db.replace_pinned(&[conv("d:a"), conv("d:empty")]).await.unwrap());
    assert_eq!(
        order(db.clone()).await,
        [("d:a".into(), true), ("d:empty".into(), true), ("d:c".into(), false), ("d:b".into(), false)]
    );
    assert!(!db.replace_pinned(&[conv("d:a"), conv("d:empty")]).await.unwrap());

    assert!(db.replace_pinned(&[conv("d:b"), conv("d:a")]).await.unwrap());
    assert_eq!(
        order(db.clone()).await,
        [("d:b".into(), true), ("d:a".into(), true), ("d:c".into(), false), ("d:empty".into(), false)]
    );
}

#[tokio::test]
async fn chat_states_apply_to_existing_conversations_only() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "hi")).await.unwrap();
    db.insert_message(&incoming("d:b", "b", 2, "hi")).await.unwrap();
    let state = |id: &str, archived, muted_until| ChatState {
        id: conv(id),
        archived,
        muted_until,
        marked_unread: false,
        blocked: false,
    };

    let past = 1_000;
    assert!(db
        .apply_chat_states(&[state("d:a", true, i64::MAX), state("d:b", false, past), state("d:new", true, 0)])
        .await
        .unwrap());
    assert!(!db.apply_chat_states(&[state("d:a", true, i64::MAX)]).await.unwrap());
    assert!(db.conversation(&conv("d:new")).await.unwrap().is_none());

    let a = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert!(a.archived && a.muted);
    // An expired mute isn't muted.
    assert!(!db.conversation(&conv("d:b")).await.unwrap().unwrap().muted);

    // Muted and archived, with muted chats kept archived: stays archived
    // when a message arrives.
    db.set_setting(KEEP_MUTED_CHATS_ARCHIVED, "true").await.unwrap();
    db.insert_message(&incoming("d:a", "a", 3, "again")).await.unwrap();
    assert!(db.conversation(&conv("d:a")).await.unwrap().unwrap().archived);
    // Not muted: a new message unarchives.
    db.apply_chat_states(&[state("d:a", true, 0)]).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 4, "once more")).await.unwrap();
    assert!(!db.conversation(&conv("d:a")).await.unwrap().unwrap().archived);
    // Muted, but muted chats aren't kept archived (Desktop's default).
    db.set_setting(KEEP_MUTED_CHATS_ARCHIVED, "false").await.unwrap();
    db.apply_chat_states(&[state("d:a", true, i64::MAX)]).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 5, "and again")).await.unwrap();
    assert!(!db.conversation(&conv("d:a")).await.unwrap().unwrap().archived);
    // Ours brings it back too, without counting as unread.
    db.apply_chat_states(&[state("d:a", true, 0)]).await.unwrap();
    let unread = db.conversation(&conv("d:a")).await.unwrap().unwrap().unread_count;
    db.insert_message(&outgoing("d:a", 6, "mine")).await.unwrap();
    let a = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert!(!a.archived);
    assert_eq!(a.unread_count, unread);
}

#[tokio::test]
async fn directory_search_skips_existing_conversations() {
    let db = test_db().await;
    let entry = |id: &str, kind, title: &str, phone: Option<&str>| DirectoryEntry {
        id: conv(id),
        kind,
        title: title.into(),
        phone: phone.map(Into::into),
    };
    db.replace_directory(&[
        entry("d:jon", ConversationKind::Direct, "Jon Smith", Some("+15551234567")),
        entry("d:ann", ConversationKind::Direct, "Ann 100% Real", None),
        entry("g:01", ConversationKind::Group, "Book club", None),
    ])
    .await
    .unwrap();
    let titles = |hits: Vec<DirectoryEntry>| hits.into_iter().map(|e| e.title).collect::<Vec<_>>();
    assert_eq!(titles(db.search_directory("jon", 10).await.unwrap()), ["Jon Smith"]);
    // Phone digits match however they're typed.
    assert_eq!(titles(db.search_directory("(555) 123", 10).await.unwrap()), ["Jon Smith"]);
    assert_eq!(titles(db.search_directory("book", 10).await.unwrap()), ["Book club"]);
    // LIKE wildcards are literal.
    assert_eq!(titles(db.search_directory("100%", 10).await.unwrap()), ["Ann 100% Real"]);
    // Once there's a conversation, it's found as a chat instead.
    db.insert_message(&incoming("d:jon", "jon", 1, "hi")).await.unwrap();
    assert!(db.search_directory("jon", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn previews_say_who_sent_it_in_groups_only() {
    let db = test_db().await;
    let preview = |db: AppDb, id: &'static str| async move {
        db.conversation(&conv(id)).await.unwrap().unwrap().last_message_preview.unwrap_or_default()
    };
    db.insert_message(&incoming("g:01", "pat", 1, "The ironic thing\nis this")).await.unwrap();
    assert_eq!(preview(db.clone(), "g:01").await, "pat: The ironic thing is this");
    db.insert_message(&outgoing("g:01", 2, "You're welcome!")).await.unwrap();
    assert_eq!(preview(db.clone(), "g:01").await, "You: You're welcome!");
    db.insert_message(&outgoing("d:josh", 3, "we typically get")).await.unwrap();
    assert_eq!(preview(db.clone(), "d:josh").await, "we typically get");
    db.insert_message(&incoming("d:josh", "josh", 4, "nice")).await.unwrap();
    assert_eq!(preview(db.clone(), "d:josh").await, "nice");
}

/// Signal Desktop's own expectations (`Crypto_test`: generateAvatarColor),
/// as indexes into A100…A210.
#[test]
fn avatar_colors_match_signal_desktop() {
    assert_eq!(avatar_color("a025bf78-653e-44e0-beb9-deb14ba32487"), 4); // A140
    assert_eq!(avatar_color("+12135550124"), 5); // A150
    assert_eq!(avatar_color("PNI:11a175e3-fe31-4eda-87da-e0bf2a2e250b"), 10); // A200
    // Group id BwJRIdomqOSOckHjnJsknNCibCZKJFt+RxLIpa9CWJ4= (base64).
    let group_id = [
        0x07, 0x02, 0x51, 0x21, 0xda, 0x26, 0xa8, 0xe4, 0x8e, 0x72, 0x41, 0xe3, 0x9c, 0x9b, 0x24, 0x9c, 0xd0, 0xa2,
        0x6c, 0x26, 0x4a, 0x24, 0x5b, 0x7e, 0x47, 0x12, 0xc8, 0xa5, 0xaf, 0x42, 0x58, 0x9e,
    ];
    assert_eq!(avatar_color_of_bytes(&group_id), 3); // A130
}

#[tokio::test]
async fn removing_messages_and_conversations() {
    let db = test_db().await;
    let first = db.insert_message(&incoming("d:a", "a", 1, "first")).await.unwrap();
    let second = db.insert_message(&incoming("d:a", "a", 2, "second")).await.unwrap();
    assert_eq!(db.remove_message(second).await.unwrap(), Some(conv("d:a")));
    let c = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!((c.last_message_preview.as_deref(), c.unread_count), (Some("first"), 1));
    assert!(db.message(second).await.unwrap().is_none());
    // Gone from search too.
    assert!(db.search_messages("second", 10).await.unwrap().is_empty());

    db.remove_conversation(&conv("d:a")).await.unwrap();
    assert!(db.conversation(&conv("d:a")).await.unwrap().is_none());
    assert!(db.message(first).await.unwrap().is_none());
}

#[tokio::test]
async fn removing_messages_through_one_keeps_newer() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "old")).await.unwrap();
    let named = db.insert_message(&incoming("d:a", "a", 2, "named")).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 3, "newer")).await.unwrap();
    db.insert_message(&incoming("d:b", "b", 1, "elsewhere")).await.unwrap();
    assert_eq!(db.remove_messages_through(&conv("d:a"), named).await.unwrap(), 1);
    let c = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!((c.last_message_preview.as_deref(), c.unread_count), (Some("newer"), 1));
    assert_eq!(db.messages(&conv("d:b"), None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn partial_chat_deletes_take_their_pins() {
    let db = test_db().await;
    let old = db.insert_message(&incoming("d:a", "a", 1, "old")).await.unwrap();
    let new = db.insert_message(&incoming("d:a", "a", 2, "new")).await.unwrap();
    db.pin_message(&conv("d:a"), old, 10, None, 3).await.unwrap();
    db.pin_message(&conv("d:a"), new, 11, None, 3).await.unwrap();
    db.remove_messages_through(&conv("d:a"), old).await.unwrap();
    let pins: Vec<i64> = db.pinned_messages(&conv("d:a")).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(pins, vec![new]);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pinned_messages").fetch_one(&db.pool).await.unwrap();
    assert_eq!(rows, 1, "no orphan pin rows");
}

#[tokio::test]
async fn clearing_forgets_the_whole_account() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "hi")).await.unwrap();
    db.replace_directory(&[DirectoryEntry {
        id: conv("d:jon"),
        kind: ConversationKind::Direct,
        title: "Jon Smith".into(),
        phone: Some("+15551234567".into()),
    }])
    .await
    .unwrap();
    db.record_emoji_use("👍").await.unwrap();
    db.clear().await.unwrap();
    assert!(db.search_directory("jon", 10).await.unwrap().is_empty());
    assert!(db.emoji_usage().await.unwrap().is_empty());
    assert!(db.conversation(&conv("d:a")).await.unwrap().is_none());
}

#[tokio::test]
async fn marked_unread_and_blocked_flags() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "hi")).await.unwrap();
    let state = ChatState { id: conv("d:a"), archived: false, muted_until: 0, marked_unread: true, blocked: true };
    assert!(db.apply_chat_states(std::slice::from_ref(&state)).await.unwrap());
    assert!(!db.apply_chat_states(&[state]).await.unwrap());
    let c = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert!(c.marked_unread && c.blocked);
    // Opening the chat clears "Mark as unread".
    db.mark_read(&conv("d:a")).await.unwrap();
    assert!(!db.conversation(&conv("d:a")).await.unwrap().unwrap().marked_unread);
    db.set_blocked(&conv("d:a"), false).await.unwrap();
    assert!(!db.conversation(&conv("d:a")).await.unwrap().unwrap().blocked);
}

#[tokio::test]
async fn disappearing_countdowns_start_on_send_and_on_read() {
    let db = test_db().await;
    let mut mine = outgoing("d:a", 1_000, "mine");
    mine.expires_in = 30;
    let mine = db.insert_message(&mine).await.unwrap();
    let mut theirs = incoming("d:a", "a", 2_000, "theirs");
    theirs.expires_in = 30;
    let theirs = db.insert_message(&theirs).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 3_000, "forever")).await.unwrap();

    // Ours counts down from sending; theirs hasn't been read.
    assert_eq!(db.message(mine).await.unwrap().unwrap().expire_start, Some(1_005)); // received_at
    assert_eq!(db.message(theirs).await.unwrap().unwrap().expire_start, None);
    assert_eq!(db.next_expiry().await.unwrap(), Some(31_005));
    assert_eq!(db.expired_messages(31_004).await.unwrap(), vec![]);
    assert_eq!(db.expired_messages(31_005).await.unwrap(), vec![(mine, conv("d:a"))]);

    db.mark_read(&conv("d:a")).await.unwrap();
    let started = db.message(theirs).await.unwrap().unwrap().expire_start.unwrap();
    assert!(started > 1_000_000_000_000, "read now: {started}");
    let far = started + 31_000;
    let expired: Vec<i64> = db.expired_messages(far).await.unwrap().into_iter().map(|(id, _)| id).collect();
    assert_eq!(expired, vec![mine, theirs], "the message without a timer stays");
}

#[tokio::test]
async fn timer_and_version_are_stored() {
    let db = test_db().await;
    db.set_expire_timer(&conv("d:a"), 3600, 4).await.unwrap();
    let c = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!((c.expire_timer, c.expire_timer_version), (3600, 4));
}

#[tokio::test]
async fn searching_one_chat_and_paging_around_a_message() {
    let db = test_db().await;
    let mut ids = Vec::new();
    for n in 1..=9 {
        ids.push(db.insert_message(&incoming("d:a", "a", n * 100, &format!("hike {n}"))).await.unwrap());
    }
    db.insert_message(&incoming("d:b", "b", 950, "hike elsewhere")).await.unwrap();

    let hits = db.search_conversation(&conv("d:a"), "hike", 50).await.unwrap();
    assert_eq!(hits.len(), 9);
    assert!(hits.iter().all(|h| h.conversation_id == conv("d:a")));
    assert_eq!(db.search_messages("hike", 50).await.unwrap().len(), 10);

    let around: Vec<i64> = db.messages_around(&conv("d:a"), ids[4], 2, 3).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(around, ids[2..=7].to_vec());
    let newer: Vec<i64> = db.messages_after(&conv("d:a"), ids[7], 5).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(newer, vec![ids[8]]);
    // A message from another chat isn't jumped into.
    let other = db.search_conversation(&conv("d:b"), "hike", 5).await.unwrap()[0].message_id;
    assert!(db.messages_around(&conv("d:a"), other, 2, 2).await.unwrap().is_empty());
}

#[tokio::test]
async fn notices_are_read_and_stay_out_of_the_preview() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 100, "hello")).await.unwrap();
    let notice = Notice::Timer { by: None, seconds: 3600 };
    let id = db.insert_notice(&conv("d:a"), 200, &notice).await.unwrap().unwrap();
    assert_eq!(db.insert_notice(&conv("d:a"), 200, &notice).await.unwrap(), None, "a repeat is ignored");
    let c = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!((c.last_message_preview.as_deref(), c.unread_count), (Some("hello"), 1));
    let shown = db.messages(&conv("d:a"), None, 10).await.unwrap();
    assert_eq!(shown.last().map(|m| (m.id, m.notice.clone())), Some((id, Some(notice))));
}

#[tokio::test]
async fn pins_keep_the_newest_three() {
    let db = test_db().await;
    let mut ids = Vec::new();
    for n in 1..=4 {
        ids.push(db.insert_message(&incoming("d:a", "a", n * 100, &format!("m{n}"))).await.unwrap());
    }
    for (n, id) in ids.iter().enumerate() {
        assert!(db.pin_message(&conv("d:a"), *id, n as i64 + 1, None, 3).await.unwrap());
    }
    let pinned: Vec<i64> = db.pinned_messages(&conv("d:a")).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(pinned, vec![ids[3], ids[2], ids[1]], "newest pin first; the oldest was replaced");
    // Expired pins don't show; removed messages lose their pin.
    db.pin_message(&conv("d:a"), ids[0], 10, Some(1), 3).await.unwrap();
    assert!(!db.pinned_messages(&conv("d:a")).await.unwrap().iter().any(|m| m.id == ids[0]));
    db.remove_message(ids[3]).await.unwrap();
    assert!(db.unpin_message(&conv("d:a"), ids[2]).await.unwrap());
    let left: Vec<i64> = db.pinned_messages(&conv("d:a")).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(left, vec![ids[1]]);
}

#[tokio::test]
async fn media_messages_are_those_with_attachments_or_previews() {
    let db = test_db().await;
    db.insert_message(&incoming("d:a", "a", 1, "just text")).await.unwrap();
    let mut photo = incoming("d:a", "a", 2, "");
    photo.attachments = vec![AttachmentMeta {
        content_type: "image/png".into(),
        file_name: None,
        size: 1,
        width: Some(1),
        height: Some(1),
        local_path: Some("/tmp/x.png".into()),
        sticker: false,
        gif: false,
        voice: false,
        remote: None,
        thumbnail_path: None,
    }];
    let photo = db.insert_message(&photo).await.unwrap();
    let media: Vec<i64> = db.media_messages(&conv("d:a"), 10).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(media, vec![photo]);
}

#[tokio::test]
async fn people_are_direct_chats_and_contacts() {
    let db = test_db().await;
    db.upsert_conversation(&conv("d:a"), ConversationUpdate { kind: Some(ConversationKind::Direct), title: Some("Alice".into()), ..Default::default() })
        .await
        .unwrap();
    db.upsert_conversation(&conv("d:z"), ConversationUpdate { kind: Some(ConversationKind::Direct), title: Some("Zed".into()), ..Default::default() })
        .await
        .unwrap();
    db.set_blocked(&conv("d:z"), true).await.unwrap();
    db.replace_directory(&[DirectoryEntry { id: conv("d:b"), kind: ConversationKind::Direct, title: "Bob".into(), phone: None }])
        .await
        .unwrap();
    let names: Vec<String> = db.people().await.unwrap().into_iter().map(|p| p.title).collect();
    assert_eq!(names, ["Alice", "Bob"], "blocked people are left out");
}

#[tokio::test]
async fn deleting_for_everyone_clears_previews_and_mentions() {
    let db = test_db().await;
    let mut message = incoming("d:a", "a", 1, "see https://example.org @Bob");
    message.link_preview = Some(LinkPreview {
        url: "https://example.org".into(),
        title: Some("Example".into()),
        description: None,
        date: None,
        image: None,
    });
    let id = db.insert_message(&message).await.unwrap();
    db.set_mentions(id, &[MentionSpan { start: 24, len: 4, aci: "b".into() }]).await.unwrap();
    db.mark_deleted(id).await.unwrap();
    let gone = db.message(id).await.unwrap().unwrap();
    assert!(gone.deleted && gone.body.is_empty());
    assert_eq!((gone.link_preview, gone.mentions.len()), (None, 0));
}

#[tokio::test]
async fn notices_dont_hide_the_last_receipt() {
    let db = test_db().await;
    let mut mine = outgoing("d:a", 100, "hi");
    mine.status = DeliveryStatus::Read;
    db.insert_message(&mine).await.unwrap();
    db.insert_notice(&conv("d:a"), 200, &Notice::Timer { by: None, seconds: 60 }).await.unwrap();
    let chat = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!(chat.last_outgoing_status, Some(DeliveryStatus::Read));
}

/// Undoes the migrations after the init, which an old database never had.
async fn drop_later_migrations(db: &AppDb) {
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version > ?").bind(INIT_VERSION).execute(&db.writer).await.unwrap();
    sqlx::query("DROP INDEX messages_author").execute(&db.writer).await.unwrap();
    sqlx::query("DROP INDEX messages_quote_author").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN styles").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN cta").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE stories").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN deleted_by_admin").execute(&db.writer).await.unwrap();
    sqlx::query("DROP INDEX messages_long_text").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN long_text").execute(&db.writer).await.unwrap();
    sqlx::query("DROP INDEX messages_polls").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN poll").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE contact_nicknames").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE early_events").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE adhoc_calls").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE call_links").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE conversations DROP COLUMN can_add_members").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE conversations DROP COLUMN group_ended").execute(&db.writer).await.unwrap();
    sqlx::query("DROP INDEX messages_view_once").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN view_once").execute(&db.writer).await.unwrap();
    sqlx::query("DROP INDEX messages_unread_poll_votes").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN unread_poll_votes").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE profile_sharing").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE story_send_modes").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE conversations DROP COLUMN group_state").execute(&db.writer).await.unwrap();
    for column in ["message_request", "reported", "added_by"] {
        sqlx::query(AssertSqlSafe(format!("ALTER TABLE conversations DROP COLUMN {column}"))).execute(&db.writer).await.unwrap();
    }
    sqlx::query("ALTER TABLE messages DROP COLUMN server_guid").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE system_contacts").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE profiles").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE badges").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE badge_image_files").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN sealed_sender").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE message_send_states").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN viewed").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN content").execute(&db.writer).await.unwrap();
    sqlx::query("DROP TABLE sticker_packs").execute(&db.writer).await.unwrap();
    sqlx::query("ALTER TABLE messages DROP COLUMN edit_history").execute(&db.writer).await.unwrap();
}

/// Rewrites the migration history to look like a pre-squash database that
/// ran the first `applied` of the 16 development migrations.
async fn fake_pre_squash_history(db: &AppDb, applied: usize) {
    const VERSIONS: [i64; 16] = [
        20261013000000, 20261014000000, 20261015000000, 20261016000000, 20261017000000, 20261018000000,
        20261019000000, 20261020000000, 20261021000000, 20261022000000, 20261023000000, 20261024000000,
        20261025000000, 20261026000000, 20261027000000, 20261028000000,
    ];
    sqlx::query("DELETE FROM _sqlx_migrations").execute(&db.writer).await.unwrap();
    // Nor had it anything from migrations after the squash.
    sqlx::query("DROP TABLE message_edits").execute(&db.writer).await.unwrap();
    drop_later_migrations(db).await;
    for version in &VERSIONS[..applied] {
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?, 'old', TRUE, x'00', 0)",
        )
        .bind(version)
        .execute(&db.writer)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn adopts_fully_migrated_pre_squash_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let key = [7u8; 32];
    {
        let db = AppDb::open(&path, &key).await.unwrap();
        db.insert_message(&incoming("d:a", "a", 1, "kept")).await.unwrap();
        fake_pre_squash_history(&db, 16).await;
        db.close().await;
    }
    let db = AppDb::open(&path, &key).await.unwrap();
    assert_eq!(db.conversations().await.unwrap().len(), 1);
    // The init stands in for the old history; later migrations then apply.
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version").fetch_all(&db.pool).await.unwrap();
    let ours: Vec<i64> = sqlx::migrate!("./migrations").iter().map(|m| m.version).collect();
    assert_eq!(versions, ours);
    // What the init gained since the old migrations is there too.
    db.edit_message(1, "edited", 2).await.unwrap();
    assert_eq!(db.find_message(&conv("d:a"), Some("a"), 2).await.unwrap().map(|m| m.body), Some("edited".into()));
    db.close().await;
    // And opens again cleanly.
    AppDb::open(&path, &key).await.unwrap().close().await;
}

#[tokio::test]
async fn rebuilds_empty_partly_migrated_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let key = [7u8; 32];
    {
        let db = AppDb::open(&path, &key).await.unwrap();
        db.set_setting("k", "v").await.unwrap();
        fake_pre_squash_history(&db, 5).await;
        db.close().await;
    }
    let db = AppDb::open(&path, &key).await.unwrap();
    assert_eq!(db.setting("k").await.unwrap(), None);
    db.insert_message(&incoming("d:a", "a", 1, "searchable")).await.unwrap();
    assert_eq!(db.search_messages("searchable", 10).await.unwrap().len(), 1);
    db.close().await;
}

#[tokio::test]
async fn refuses_partly_migrated_database_with_chats() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let key = [7u8; 32];
    {
        let db = AppDb::open(&path, &key).await.unwrap();
        db.insert_message(&incoming("d:a", "a", 1, "kept")).await.unwrap();
        fake_pre_squash_history(&db, 5).await;
        db.close().await;
    }
    assert!(matches!(AppDb::open(&path, &key).await, Err(Error::Schema(_))));
}

#[tokio::test]
async fn an_edited_message_answers_to_its_edits_timestamps() {
    let db = test_db().await;
    let theirs = db.insert_message(&incoming("d:a", "a", 100, "hi")).await.unwrap();
    db.edit_message(theirs, "hi there", 200).await.unwrap();
    // A reaction (or reply) to the edited message names the edit.
    let found = db.find_message(&conv("d:a"), Some("a"), 200).await.unwrap().unwrap();
    assert_eq!(found.id, theirs);
    assert_eq!(db.find_message_any(Some("a"), 200).await.unwrap().map(|m| m.id), Some(theirs));
    assert_eq!(db.find_message(&conv("d:a"), Some("a"), 100).await.unwrap().map(|m| m.id), Some(theirs));
    assert!(db.find_message(&conv("d:a"), Some("b"), 200).await.unwrap().is_none(), "another author's message");
    // A read sync for the edit reads it.
    assert_eq!(db.mark_message_read("a", 200).await.unwrap(), Some(conv("d:a")));

    // Our own: receipts for the edit's timestamp move its status.
    let mut ours = incoming("d:a", "a", 300, "typo");
    ours.direction = Direction::Outgoing;
    ours.author = None;
    ours.status = DeliveryStatus::Sent;
    let ours = db.insert_message(&ours).await.unwrap();
    db.edit_message(ours, "fixed", 400).await.unwrap();
    let changed = db.update_status_by_sent_at(400, DeliveryStatus::Delivered).await.unwrap();
    assert_eq!(changed, [(conv("d:a"), ours)]);

    // A message's own timestamp wins over another's edit at the same time.
    let later = db.insert_message(&incoming("d:a", "a", 200, "another")).await.unwrap();
    assert_eq!(db.find_message(&conv("d:a"), Some("a"), 200).await.unwrap().map(|m| m.id), Some(later));

    // Removing a message drops its edits.
    db.remove_message(theirs).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_edits WHERE message_id = ?")
        .bind(theirs)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A database made by the first init (`message_edits` came later), with
/// or without the `message_edits` migration applied.
async fn fake_first_init(db: &AppDb, with_edits_migration: bool) {
    drop_later_migrations(db).await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00'").execute(&db.writer).await.unwrap();
    if with_edits_migration {
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (20261029000000, 'message edits', TRUE, x'00', 0)",
        )
        .execute(&db.writer)
        .await
        .unwrap();
    } else {
        sqlx::query("DROP TABLE message_edits").execute(&db.writer).await.unwrap();
    }
}

#[tokio::test]
async fn adopts_a_database_from_the_first_init() {
    for with_edits_migration in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        let key = [7u8; 32];
        {
            let db = AppDb::open(&path, &key).await.unwrap();
            db.insert_message(&incoming("d:a", "a", 1, "kept")).await.unwrap();
            fake_first_init(&db, with_edits_migration).await;
            db.close().await;
        }
        let db = AppDb::open(&path, &key).await.unwrap();
        assert_eq!(db.conversations().await.unwrap().len(), 1, "with_edits_migration: {with_edits_migration}");
        let id = db.find_message(&conv("d:a"), Some("a"), 1).await.unwrap().unwrap().id;
        db.edit_message(id, "edited", 2).await.unwrap();
        assert_eq!(db.find_message(&conv("d:a"), Some("a"), 2).await.unwrap().map(|m| m.id), Some(id));
        // The init stands in for the old history; later migrations then apply.
        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version").fetch_all(&db.pool).await.unwrap();
        let ours: Vec<i64> = sqlx::migrate!("./migrations").iter().map(|m| m.version).collect();
        assert_eq!(versions, ours);
        db.close().await;
        AppDb::open(&path, &key).await.unwrap().close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_wait_for_each_other() {
    // As during a history import, with messages arriving at the same time:
    // transactions that read before writing used to fail with "database is
    // locked" when another connection committed in between.
    let dir = tempfile::tempdir().unwrap();
    let db = AppDb::open(&dir.path().join("app.db"), &[7u8; 32]).await.unwrap();
    let tasks: Vec<_> = (0..8)
        .map(|writer| {
            let db = db.clone();
            tokio::spawn(async move {
                for n in 0..40 {
                    let chat = format!("d:{writer}");
                    let id = db.insert_message(&incoming(&chat, "a", n, "hi")).await?;
                    // Reads the reactions, then writes them.
                    db.set_reaction(id, Reaction { emoji: "👍".into(), author: None, sent_at: n }).await?;
                }
                Ok::<_, Error>(())
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(db.conversations().await.unwrap().len(), 8);
}

#[tokio::test]
async fn a_read_sync_reads_everything_older_in_its_chat() {
    let db = test_db().await;
    for (author, at) in [("a", 1), ("b", 2), ("a", 3), ("a", 4)] {
        db.insert_message(&incoming("g:00ff", author, at, "hi")).await.unwrap();
    }
    db.insert_message(&incoming("d:other", "a", 2, "elsewhere")).await.unwrap();
    // The phone sent one read sync, for the third message.
    assert_eq!(db.apply_read_sync("a", 3).await.unwrap(), ReadSync::Read(conv("g:00ff")));
    let unread: Vec<i64> = sqlx::query_scalar("SELECT sent_at FROM messages WHERE conversation_id = 'g:00ff' AND is_read = 0")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(unread, [4]);
    assert_eq!(db.conversation(&conv("g:00ff")).await.unwrap().unwrap().unread_count, 1);
    assert_eq!(db.conversation(&conv("d:other")).await.unwrap().unwrap().unread_count, 1, "other chats untouched");
    assert_eq!(db.apply_read_sync("a", 3).await.unwrap(), ReadSync::AlreadyRead);
    // Before its message arrives.
    assert_eq!(db.apply_read_sync("a", 99).await.unwrap(), ReadSync::NotFound);
}

#[tokio::test]
async fn safety_number_notices_round_trip() {
    let db = test_db().await;
    let notice = Notice::SafetyNumberChanged { who: "a".into() };
    let id = db.insert_notice(&conv("d:a"), 5, &notice).await.unwrap().unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().notice, Some(notice));
}

#[tokio::test]
async fn a_call_seen_here_and_on_another_device_is_one_notice() {
    let db = test_db().await;
    let call = |outcome, unseen| Notice::Call { incoming: true, video: false, outcome, call_id: Some(-7), unseen };
    // It rang here and went unanswered: a missed call, not looked at yet…
    let id = db.record_call(&conv("d:a"), 1_000, &call(CallOutcome::Missed, false)).await.unwrap().unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().notice, Some(call(CallOutcome::Missed, true)));
    assert_eq!(db.unseen_calls().await.unwrap(), 1);
    // …because the phone answered it (its time differs a little).
    let again = db.record_call(&conv("d:a"), 1_200, &call(CallOutcome::Accepted, false)).await.unwrap().unwrap();
    assert_eq!(again, id);
    assert_eq!(db.message(id).await.unwrap().unwrap().notice, Some(call(CallOutcome::Accepted, false)));
    assert_eq!(db.unseen_calls().await.unwrap(), 0);
    // Seen stays seen.
    db.record_call(&conv("d:a"), 1_200, &call(CallOutcome::Missed, false)).await.unwrap();
    assert!(!db.message(id).await.unwrap().unwrap().notice.unwrap().call_unseen());
    // Another call at the same moment is its own notice.
    let other = Notice::Call { incoming: false, video: true, outcome: CallOutcome::Unanswered, call_id: Some(8), unseen: false };
    let second = db.record_call(&conv("d:a"), 1_000, &other).await.unwrap().unwrap();
    assert_ne!(second, id);
    // Deleted from the call history elsewhere.
    assert_eq!(db.remove_call(&conv("d:a"), -7).await.unwrap(), Some(id));
    assert!(db.message(id).await.unwrap().is_none());
}

fn group_call(call_id: i64, era: Option<&str>, by: Option<&str>, at: i64) -> GroupCallRecord {
    GroupCallRecord { call_id, era: era.map(str::to_owned), by: by.map(str::to_owned), incoming: by.is_some(), at }
}

#[tokio::test]
async fn a_group_call_is_one_record_that_follows_desktops_statuses() {
    let db = test_db().await;
    let g = conv("g:a");
    let rung = group_call(42, None, Some("alice"), 1_000);
    // Alice rings us: unseen until we look.
    let change = db.apply_group_call_event(&g, &rung, CallHistoryEvent::Ringing, false, false).await.unwrap().unwrap();
    assert_eq!((change.status, change.previous), (GroupCallStatus::Ringing, None));
    assert_eq!(db.unseen_calls().await.unwrap(), 1);
    // Bob's update about the same call (by its era) adds nothing.
    let update = group_call(42, Some("e1"), Some("bob"), 2_000);
    assert_eq!(db.apply_group_call_event(&g, &update, CallHistoryEvent::Started, false, true).await.unwrap(), None);
    // We join: accepted, and seen.
    let joined = db.apply_group_call_event(&g, &update, CallHistoryEvent::Accepted, true, false).await.unwrap().unwrap();
    assert_eq!((joined.id, joined.status, joined.previous), (change.id, GroupCallStatus::Accepted, Some(GroupCallStatus::Ringing)));
    let notice = db.message(change.id).await.unwrap().unwrap().notice.unwrap();
    assert_eq!(
        notice,
        Notice::GroupCall {
            by: Some("alice".into()),
            era: Some("e1".into()),
            call_id: Some(42),
            status: Some(GroupCallStatus::Accepted),
            incoming: true,
            unseen: false,
        }
    );
    assert_eq!(db.unseen_calls().await.unwrap(), 0);
    // A notice from before call ids is found by its era.
    let old = Notice::GroupCall { by: Some("bob".into()), era: Some("e2".into()), call_id: None, status: None, incoming: false, unseen: false };
    let old_id = db.insert_notice(&g, 5_000, &old).await.unwrap().unwrap();
    let change = db
        .apply_group_call_event(&g, &group_call(43, Some("e2"), None, 6_000), CallHistoryEvent::Accepted, true, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((change.id, change.status), (old_id, GroupCallStatus::Joined));
    // The same call id in another group is another call.
    let other = db.apply_group_call_event(&conv("g:b"), &rung, CallHistoryEvent::Missed, false, false).await.unwrap().unwrap();
    assert_eq!(other.previous, None);
    assert_eq!(db.remove_group_call(&conv("g:b"), 42).await.unwrap(), Some(other.id));
}

#[tokio::test]
async fn calls_follow_the_chats_disappearing_timer_once_seen() {
    let db = test_db().await;
    let g = conv("g:a");
    db.upsert_conversation(&g, ConversationUpdate { expire_timer: Some(60), ..Default::default() }).await.unwrap();
    // Ours, seen: counts down from now, unless we're still in the call.
    let ours = db
        .apply_group_call_event(&g, &group_call(1, Some("e1"), None, 1_000), CallHistoryEvent::Ringing, true, false)
        .await
        .unwrap()
        .unwrap();
    let message = db.message(ours.id).await.unwrap().unwrap();
    assert_eq!((message.expires_in, message.expire_start), (60, None));
    // Leaving starts it.
    db.apply_group_call_event(&g, &group_call(1, Some("e1"), None, 1_000), CallHistoryEvent::Started, false, false)
        .await
        .unwrap();
    assert!(db.message(ours.id).await.unwrap().unwrap().expire_start.is_some());
    // A missed one waits until the call history is read…
    let missed = db
        .apply_group_call_event(&g, &group_call(2, None, Some("alice"), 2_000), CallHistoryEvent::Missed, false, false)
        .await
        .unwrap()
        .unwrap();
    let in_call = db
        .apply_group_call_event(&g, &group_call(3, None, Some("alice"), 3_000), CallHistoryEvent::Accepted, true, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(db.message(missed.id).await.unwrap().unwrap().expire_start, None);
    let target = CallTarget { conversation: Some(g.clone()), call_id: Some(3), timestamp: 0 };
    let changed = db.mark_calls_seen(&target, true, 9_000, Some((&g, 3))).await.unwrap();
    assert_eq!(changed, [(g.clone(), missed.id)]);
    let message = db.message(missed.id).await.unwrap().unwrap();
    assert_eq!(message.expire_start, Some(9_000));
    assert!(!message.notice.unwrap().call_unseen());
    // …but not the call we're in.
    assert_eq!(db.message(in_call.id).await.unwrap().unwrap().expire_start, None);
    // A 1:1 call takes its chat's timer too.
    db.upsert_conversation(&conv("d:a"), ConversationUpdate { expire_timer: Some(30), ..Default::default() }).await.unwrap();
    let call = Notice::Call { incoming: false, video: false, outcome: CallOutcome::Accepted, call_id: Some(4), unseen: false };
    let id = db.record_call(&conv("d:a"), 4_000, &call).await.unwrap().unwrap();
    let message = db.message(id).await.unwrap().unwrap();
    assert_eq!(message.expires_in, 30);
    assert!(message.expire_start.is_some());
}

#[tokio::test]
async fn the_call_history_clears_and_reads_up_to_a_call() {
    let db = test_db().await;
    let missed = |call_id| Notice::Call { incoming: true, video: false, outcome: CallOutcome::Missed, call_id: Some(call_id), unseen: false };
    db.record_call(&conv("d:a"), 1_000, &missed(1)).await.unwrap();
    db.record_call(&conv("d:b"), 2_000, &missed(2)).await.unwrap();
    let last = db.record_call(&conv("d:a"), 3_000, &missed(3)).await.unwrap().unwrap();
    db.insert_message(&incoming("d:a", "a", 1_500, "hi")).await.unwrap();
    assert_eq!(db.unseen_calls().await.unwrap(), 3);
    let latest = db.latest_call(None).await.unwrap().unwrap();
    assert_eq!((latest.conversation, latest.call_id, latest.sent_at), (conv("d:a"), Some(3), 3_000));
    assert_eq!(db.latest_unseen_call(&conv("d:b")).await.unwrap().map(|c| c.call_id), Some(Some(2)));
    // Read everywhere up to the second call (named by its time).
    let target = CallTarget { conversation: None, call_id: None, timestamp: 2_500 };
    assert_eq!(db.mark_calls_seen(&target, false, 5_000, None).await.unwrap().len(), 2);
    assert_eq!(db.unseen_calls().await.unwrap(), 1);
    // A call that isn't here names nothing.
    let unknown = CallTarget { conversation: Some(conv("d:a")), call_id: Some(99), timestamp: 9_000 };
    assert!(db.clear_call_history(&unknown).await.unwrap().is_empty());
    // Cleared up to the second call: the third stays, and so do messages.
    let target = CallTarget { conversation: Some(conv("d:b")), call_id: Some(2), timestamp: 0 };
    assert_eq!(db.clear_call_history(&target).await.unwrap().len(), 2);
    let left: Vec<i64> = db.call_history(10).await.unwrap().into_iter().map(|m| m.id).collect();
    assert_eq!(left, [last]);
    assert_eq!(db.messages(&conv("d:a"), None, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_call_history_reads_only_notices() {
    let db = test_db().await;
    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM messages WHERE author_key = 'notice' \
           AND json_extract(notice, '$.type') IN ('call', 'group_call') AND json_extract(notice, '$.unseen') = 1",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    // Through the author index (notices are a few of all messages), not a
    // scan of every message.
    assert!(plan.iter().any(|(_, _, _, detail)| detail.contains("messages_author")), "{plan:?}");
}

#[tokio::test]
async fn writes_queue_rather_than_fail_while_another_holds_the_lock() {
    let db = test_db().await;
    // A long write: a transaction kept open for a while.
    let mut tx = db.write().await.unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('a', '1')").execute(&mut *tx).await.unwrap();
    let other = db.clone();
    let queued = tokio::spawn(async move { other.set_setting("b", "2").await });
    // Past the busy timeout's 5 seconds would fail it; queued, it waits.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!queued.is_finished(), "waits its turn");
    tx.commit().await.unwrap();
    queued.await.unwrap().unwrap();
    assert_eq!(db.setting("b").await.unwrap().as_deref(), Some("2"));
    // Reads go on meanwhile, and can't write.
    assert!(sqlx::query("DELETE FROM settings").execute(&db.pool).await.is_err());
}

#[tokio::test]
async fn renaming_an_author_looks_up_their_messages_by_index() {
    let db = test_db().await;
    let plan = |sql: &'static str| {
        let db = db.clone();
        async move {
            let rows: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}"))).fetch_all(&db.pool).await.unwrap();
            rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ")
        }
    };
    let names = plan("UPDATE messages SET author_name = 'x' WHERE author_key = 'a' AND author_name IS NOT 'x'").await;
    assert!(names.contains("messages_author"), "{names}");
    let quotes = plan(
        "UPDATE messages SET quote = json_set(quote, '$.author_name', 'x') \
         WHERE quote IS NOT NULL AND json_extract(quote, '$.author') = 'a' AND json_extract(quote, '$.author_name') IS NOT 'x'",
    )
    .await;
    assert!(quotes.contains("messages_quote_author"), "{quotes}");
}

#[tokio::test]
async fn styles_are_kept_and_spoilers_stay_out_of_previews() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("d:a", "a", 1, "the ending: he was a ghost")).await.unwrap();
    let styles = [
        StyleSpan { start: 0, len: 3, style: TextStyle::Bold },
        StyleSpan { start: 12, len: 14, style: TextStyle::Spoiler },
    ];
    db.set_styles(id, &styles).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().styles, styles);
    let chat = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!(chat.last_message_preview.as_deref(), Some("the ending: ■■■■"));
    // Edited: the old formatting doesn't fit the new text.
    db.edit_message(id, "the ending is a surprise", 2).await.unwrap();
    assert!(db.message(id).await.unwrap().unwrap().styles.is_empty());
    db.set_styles(id, &styles[..1]).await.unwrap();
    // Deleted for everyone: its formatting goes too.
    db.mark_deleted(id).await.unwrap();
    assert!(db.message(id).await.unwrap().unwrap().styles.is_empty());
}

#[test]
fn spoilers_hide_whole_and_overlapping() {
    let spoiler = |start, len| StyleSpan { start, len, style: TextStyle::Spoiler };
    assert_eq!(hide_spoilers("a secret b", &[spoiler(2, 6)]), "a ■■■■ b");
    assert_eq!(hide_spoilers("abcdef", &[spoiler(0, 3), spoiler(2, 2)]), "■■■■ef");
    assert_eq!(hide_spoilers("abc", &[spoiler(1, 9)]), "abc", "out of range: left alone");
    let bold = StyleSpan { start: 0, len: 3, style: TextStyle::Bold };
    assert_eq!(hide_spoilers("abc", &[bold]), "abc");
}

#[test]
fn quoted_attachments_say_what_they_are_as_desktop_does() {
    let quoted = |content_type: &str, voice: bool, gif: bool, file_name: Option<&str>| {
        QuotedAttachment { content_type: content_type.into(), file_name: file_name.map(Into::into), voice, gif, view_once: false }.label()
    };
    assert_eq!(QuotedAttachment::view_once().label(), "View-once Photo", "as Desktop quotes one: a JPEG");
    assert_eq!(quoted("audio/aac", true, false, None), "Voice Message");
    assert_eq!(quoted("audio/mpeg", false, false, Some("song.mp3")), "Audio");
    assert_eq!(quoted("image/jpeg", false, false, None), "Photo");
    assert_eq!(quoted("video/mp4", false, true, None), "GIF");
    assert_eq!(quoted("video/mp4", false, false, None), "Video");
    assert_eq!(quoted("application/pdf", false, false, Some("notes.pdf")), "notes.pdf");
    // Quotes stored before this knew nothing of the attachment.
    let old: Quote = serde_json::from_str(r#"{"author":null,"author_name":"You","sent_at":1,"text":""}"#).unwrap();
    assert_eq!(old.attachment, None);
}

#[tokio::test]
async fn release_notes_keep_their_button() {
    let db = test_db().await;
    let id = db.insert_message(&incoming(&ConversationId::signal().0, SIGNAL_ACI, 1, "Hello\n\nNews")).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().cta, None);
    let cta = CallToAction { id: "donate".into(), text: Some("Donate Now".into()) };
    db.set_cta(id, &cta).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().cta, Some(cta));
    assert!(ConversationId::signal().is_signal() && !conv("d:a").is_signal());
    // The Signal chat isn't someone to message or add to a group.
    db.upsert_conversation(&ConversationId::signal(), ConversationUpdate { title: Some("Signal".into()), ..Default::default() })
        .await
        .unwrap();
    assert!(db.people().await.unwrap().iter().all(|p| !p.id.is_signal()));
}

#[tokio::test]
async fn knows_when_it_was_made() {
    let db = test_db().await;
    let made = db.created_at().await.unwrap().expect("a time");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!((now - made).abs() < 60_000, "{made} vs {now}");
}

#[tokio::test]
async fn attachments_are_named_as_desktop_names_them() {
    let meta = |content_type: &str| AttachmentMeta {
        content_type: content_type.into(),
        file_name: None,
        size: 1,
        width: None,
        height: None,
        local_path: None,
        sticker: false,
        gif: false,
        voice: false,
        remote: None,
        thumbnail_path: None,
    };
    let gif = AttachmentMeta { gif: true, ..meta("video/mp4") };
    assert_eq!(attachment_preview(&gif), "🎡 GIF", "a GIF is an MP4 with the GIF flag");
    assert_eq!(attachment_preview(&meta("video/mp4")), "🎥 Video");
    assert_eq!(attachment_preview(&meta("image/gif")), "🎡 GIF");
    assert_eq!(attachment_preview(&meta("image/jpeg")), "📷 Photo");
    assert_eq!(attachment_preview(&AttachmentMeta { voice: true, ..meta("audio/aac") }), "🎤 Voice Message");
    assert_eq!(attachment_preview(&meta("audio/mpeg")), "🔈 Audio Message");
    assert_eq!(attachment_preview(&AttachmentMeta { sticker: true, ..meta("image/webp") }), "Sticker message");
    assert_eq!(attachment_preview(&meta("application/pdf")), "📎 File");

    // The chat list says so too.
    let db = test_db().await;
    let mut m = incoming("d:a", "a", 1, "");
    m.attachments = vec![gif];
    db.insert_message(&m).await.unwrap();
    let chat = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!(chat.last_message_preview.as_deref(), Some("🎡 GIF"));
}

#[tokio::test]
async fn stories_last_a_day_and_keep_views_and_replies() {
    let db = test_db().await;
    let now = 10 * STORY_LIFETIME_MS;
    let text = |text: &str| StoryContent::Text(TextStory { text: text.into(), ..Default::default() });
    let story = |author: Option<&str>, sent_at: i64, content: StoryContent| NewStory {
        author: author.map(Into::into),
        author_name: author.map(|_| "Alice".into()),
        sent_at,
        received_at: sent_at,
        group: None,
        content,
        pointer: None,
        allows_replies: true,
        read: false,
        recipients: Vec::new(),
    };
    let theirs = db.insert_story(&story(Some("a"), now - 1_000, text("hi")), now).await.unwrap().unwrap();
    assert_eq!(db.insert_story(&story(Some("a"), now - 1_000, text("hi")), now).await.unwrap(), None, "once");
    assert_eq!(db.insert_story(&story(Some("a"), now - STORY_LIFETIME_MS, text("old")), now).await.unwrap(), None, "already gone");
    let photo = StoryContent::Media {
        attachment: AttachmentMeta {
            content_type: "image/jpeg".into(),
            file_name: None,
            size: 1,
            width: None,
            height: None,
            local_path: None,
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        },
        caption: String::new(),
        styles: Vec::new(),
    };
    let ours = db.insert_story(&story(None, now - 500, photo), now).await.unwrap().unwrap();
    db.set_story_file(ours, "/tmp/x.jpg").await.unwrap();

    let stories = db.stories(now).await.unwrap();
    assert_eq!(stories.iter().map(|s| s.id).collect::<Vec<_>>(), [ours, theirs], "newest first");
    assert!(matches!(&stories[0].content, StoryContent::Media { attachment, .. } if attachment.local_path.as_deref() == Some("/tmp/x.jpg")));

    assert!(db.mark_story_read(Some("a"), now - 1_000).await.unwrap());
    assert!(!db.mark_story_read(Some("a"), now - 1_000).await.unwrap(), "once");
    assert!(db.add_story_view(now - 500, "b", now - 400).await.unwrap());
    assert!(db.add_story_view(now - 500, "b", now - 300).await.unwrap());
    assert!(!db.add_story_view(now - 1_000, "b", now).await.unwrap(), "not ours");
    let reply = StoryReply { author: Some("b".into()), author_name: None, sent_at: now, body: "nice".into(), emoji: None, deleted: false };
    assert_eq!(db.add_story_reply(Some("a"), now - 1_000, &reply).await.unwrap(), Some(theirs));
    assert_eq!(db.add_story_reply(Some("a"), 1, &reply).await.unwrap(), None, "no such story");
    let theirs_now = db.story(theirs).await.unwrap().unwrap();
    assert!(theirs_now.read);
    assert_eq!(theirs_now.replies, std::slice::from_ref(&reply));
    // Deleted by its author for everyone: kept, marked; for me: gone.
    assert!(db.remove_story_reply(theirs, Some("b"), now, true).await.unwrap());
    let kept = &db.story(theirs).await.unwrap().unwrap().replies[0];
    assert!(kept.deleted && kept.body.is_empty());
    assert!(!db.remove_story_reply(theirs, Some("c"), now, false).await.unwrap(), "no such reply");
    assert!(db.remove_story_reply(theirs, Some("b"), now, false).await.unwrap());
    assert!(db.story(theirs).await.unwrap().unwrap().replies.is_empty());
    db.add_story_reply(Some("a"), now - 1_000, &reply).await.unwrap();
    assert_eq!(db.story(ours).await.unwrap().unwrap().views, [StoryView { viewer: "b".into(), at: Some(now - 400) }], "the first time");

    // A day after: gone, with the photo's file to delete.
    let later = now - 500 + STORY_LIFETIME_MS;
    assert_eq!(db.next_story_expiry().await.unwrap(), Some(now - 1_000 + STORY_LIFETIME_MS));
    assert_eq!(db.remove_expired_stories(later).await.unwrap(), ["/tmp/x.jpg"]);
    assert!(db.stories(later).await.unwrap().is_empty());
}

#[tokio::test]
async fn our_stories_keep_who_they_went_to() {
    let db = test_db().await;
    let now = 10 * STORY_LIFETIME_MS;
    let recipient = |sid: &str, lists: &[&str]| StoryRecipient {
        service_id: sid.into(),
        list_ids: lists.iter().map(|l| (*l).to_owned()).collect(),
        allows_replies: true,
    };
    let story = NewStory {
        author: None,
        author_name: None,
        sent_at: now - 10,
        received_at: now - 10,
        group: None,
        content: StoryContent::Text(TextStory { text: "hi".into(), ..Default::default() }),
        pointer: None,
        allows_replies: true,
        read: true,
        recipients: vec![recipient("a", &["my"]), recipient("b", &["my", "close"])],
    };
    let id = db.insert_story(&story, now).await.unwrap().unwrap();
    assert_eq!(db.story(id).await.unwrap().unwrap().recipients, story.recipients);
    db.set_story_recipients(id, &[recipient("b", &["my"])]).await.unwrap();
    assert_eq!(db.story(id).await.unwrap().unwrap().recipients, [recipient("b", &["my"])]);

    // Views kept as plain strings, before their times were, still read.
    sqlx::query("UPDATE stories SET views = '[\"c\"]' WHERE id = ?").bind(id).execute(&db.writer).await.unwrap();
    assert_eq!(db.story(id).await.unwrap().unwrap().views, [StoryView { viewer: "c".into(), at: None }]);
    assert!(db.add_story_view(now - 10, "d", now).await.unwrap());
    assert_eq!(db.story(id).await.unwrap().unwrap().views.len(), 2);

    assert_eq!(db.remove_story(id).await.unwrap(), None, "a text story has no file");
    assert_eq!(db.story(id).await.unwrap(), None);
    assert_eq!(db.remove_story(id).await.unwrap(), None);
}

#[tokio::test]
async fn profile_sharing_and_story_send_modes_are_kept_per_chat() {
    let db = test_db().await;
    let (alice, bob, group) = (ConversationId::direct("a"), ConversationId::direct("b"), conv("g"));
    assert!(!db.profile_sharing(&alice).await.unwrap());
    assert!(db.set_profile_sharing(&alice, true).await.unwrap());
    assert!(!db.set_profile_sharing(&alice, true).await.unwrap(), "once");
    assert!(db.profile_sharing(&alice).await.unwrap());
    // Storage Service's word, for the records it has.
    db.apply_profile_sharing(&[(alice.clone(), false), (bob.clone(), true)]).await.unwrap();
    assert!(!db.profile_sharing(&alice).await.unwrap());
    assert!(db.profile_sharing(&bob).await.unwrap());

    assert_eq!(db.story_send_mode(&group).await.unwrap(), StorySendMode::IfActive);
    assert!(db.set_story_send_modes(&[(group.clone(), StorySendMode::Always)]).await.unwrap());
    assert!(!db.set_story_send_modes(&[(group.clone(), StorySendMode::Always)]).await.unwrap(), "unchanged");
    assert_eq!(db.story_send_modes().await.unwrap(), [(group.clone(), StorySendMode::Always)]);
    assert!(db.set_story_send_modes(&[(group.clone(), StorySendMode::Never)]).await.unwrap());
    assert_eq!(db.story_send_mode(&group).await.unwrap(), StorySendMode::Never);
    // Back to the default: not listed.
    assert!(db.set_story_send_modes(&[(group.clone(), StorySendMode::IfActive)]).await.unwrap());
    assert!(db.story_send_modes().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_long_text_replaces_the_start_of_the_body_once() {
    let db = test_db().await;
    let long_text = |cdn_key: &str| LongText {
        remote: RemoteAttachment {
            cdn: 2,
            cdn_key: Some(cdn_key.into()),
            key: "00".repeat(64),
            digest: Some("11".repeat(32)),
            plaintext_hash: None,
            media_tier_cdn: None,
        },
        size: 3000,
        mentions: vec!["m".into()],
        styles: vec![WireStyle { start: 0, len: 2500, style: TextStyle::Bold }],
        state: LongTextState::Downloading,
        attempts: 0,
        retry_at: None,
    };
    let first = long_text("first");
    let id = db.insert_message(&incoming("d:a", "a", 1, "the start")).await.unwrap();
    db.set_long_text(id, Some(&first)).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().long_text.as_ref(), Some(&first));
    assert_eq!(db.long_texts_to_resume(10).await.unwrap(), [(id, None)]);

    // A failed download is tried again later, and resumes at that time.
    assert!(db.set_long_text_state(id, &first, LongTextState::Failed, 1, Some(500)).await.unwrap());
    let failed = db.message(id).await.unwrap().unwrap().long_text.unwrap();
    assert_eq!((failed.state, failed.attempts, failed.retry_at), (LongTextState::Failed, 1, Some(500)));
    assert_eq!(db.long_texts_to_resume(10).await.unwrap(), [(id, Some(500))]);
    // Out of tries, it waits to be asked for.
    assert!(db.set_long_text_state(id, &first, LongTextState::Failed, 5, None).await.unwrap());
    assert!(db.long_texts_to_resume(10).await.unwrap().is_empty());

    // An edit since brings its own text: the old one no longer applies.
    db.edit_message(id, "edited start", 2).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().long_text, None, "an edit clears it");
    let second = long_text("second");
    db.set_long_text(id, Some(&second)).await.unwrap();
    assert!(!db.set_long_text_state(id, &first, LongTextState::Failed, 1, None).await.unwrap());
    assert!(!db.apply_long_text(id, &first, "old whole text", &[], &[]).await.unwrap());

    let whole = format!("whole {} and the rest", "x".repeat(2500));
    let styles = [StyleSpan { start: 6, len: 2500, style: TextStyle::Bold }];
    let mentions = [MentionSpan { start: 2507, len: 4, aci: "m".into() }];
    assert!(db.apply_long_text(id, &second, &whole, &mentions, &styles).await.unwrap());
    let got = db.message(id).await.unwrap().unwrap();
    assert_eq!((got.body.as_str(), got.long_text), (whole.as_str(), None));
    assert_eq!((got.styles.as_slice(), got.mentions.as_slice()), (&styles[..], &mentions[..]));
    assert!(got.edited, "still marked edited, by the edit");
    assert_eq!(got.edit_count, 1, "the text isn't another edit");
    assert_eq!(db.search_messages("rest", 10).await.unwrap().len(), 1, "searchable");
    assert!(
        db.conversation(&conv("d:a")).await.unwrap().unwrap().last_message_preview.unwrap().starts_with("whole x"),
        "the chat list shows it"
    );
    // Applied once.
    assert!(!db.apply_long_text(id, &second, "again", &[], &[]).await.unwrap());

    // A deleted message takes none.
    let gone = db.insert_message(&incoming("d:a", "a", 3, "start")).await.unwrap();
    db.set_long_text(gone, Some(&first)).await.unwrap();
    db.mark_deleted(gone).await.unwrap();
    assert_eq!(db.message(gone).await.unwrap().unwrap().long_text, None);
    assert!(!db.apply_long_text(gone, &first, "whole", &[], &[]).await.unwrap());
    assert!(db.long_texts_to_resume(10).await.unwrap().is_empty());
}

fn vote(author: Option<&str>, options: &[u32], vote_count: u32, sent_at: i64) -> PollVote {
    PollVote { author: author.map(Into::into), options: options.to_vec(), vote_count, sent_at, ..Default::default() }
}

fn lunch() -> Poll {
    Poll::new("Lunch?".into(), vec!["Tacos".into(), "Pho".into(), "Salad".into()], false)
}

#[test]
fn poll_votes_follow_desktops_rules() {
    let mut poll = lunch();
    // Not its options, the same one twice, or two in a single-choice poll.
    assert!(!poll.apply_vote(vote(Some("a"), &[3], 1, 10), VoteSource::Other));
    assert!(!poll.apply_vote(vote(Some("a"), &[0, 0], 1, 10), VoteSource::Other));
    assert!(!poll.apply_vote(vote(Some("a"), &[0, 1], 1, 10), VoteSource::Other));
    assert!(poll.apply_vote(vote(Some("a"), &[0], 2, 10), VoteSource::Other));
    // A lower or equal count is older; from our other device a later one
    // with the same count wins.
    assert!(!poll.apply_vote(vote(Some("a"), &[1], 1, 20), VoteSource::Other));
    assert!(!poll.apply_vote(vote(Some("a"), &[1], 2, 20), VoteSource::Other));
    assert!(poll.apply_vote(vote(Some("a"), &[1], 3, 20), VoteSource::Other));
    assert!(poll.apply_vote(vote(None, &[2], 1, 30), VoteSource::Sync));
    assert!(poll.apply_vote(vote(None, &[1], 1, 40), VoteSource::Sync));
    assert!(!poll.apply_vote(vote(None, &[0], 1, 35), VoteSource::Sync));
    let tally = poll.tally();
    assert_eq!(tally.voters, vec![vec![], vec![Some("a".into()), None], vec![]]);
    assert_eq!((tally.total_votes, tally.unique_voters), (2, 2));
    assert_eq!(tally.ours, [1].into());
    // Taking a vote back: no options.
    assert!(poll.apply_vote(vote(Some("a"), &[], 4, 50), VoteSource::Other));
    assert_eq!(poll.tally().count(1), 1);
    // Ended: no more votes.
    assert!(poll.end());
    assert!(!poll.end());
    assert!(!poll.apply_vote(vote(Some("b"), &[0], 1, 60), VoteSource::Other));
}

/// Ours from here, to `to`.
fn our_vote(options: &[u32], vote_count: u32, sent_at: i64, to: &[&str]) -> PollVote {
    PollVote { sends: to.iter().map(|t| (t.to_string(), false)).collect(), ..vote(None, options, vote_count, sent_at) }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn our_votes_count_once_sent_to_everyone() {
    let mut poll = Poll { allow_multiple: true, ..lunch() };
    assert_eq!(poll.next_vote_count(), 1);
    assert!(poll.apply_vote(our_vote(&[0, 2], 1, 10, &["a", "b"]), VoteSource::ThisDevice));
    let tally = poll.tally();
    assert_eq!((tally.total_votes, tally.chosen().clone()), (0, [0, 2].into()), "pending: shown, not counted");
    // Reaching some keeps it pending, waiting for the rest.
    assert!(poll.record_vote_sends(1, &names(&["a"]), &[]));
    assert_eq!(poll.our_pending_vote().map(PollVote::unsent), Some(names(&["b"])));
    assert!(poll.record_vote_sends(1, &names(&["b"]), &[]));
    assert_eq!(poll.our_pending_vote(), None);
    assert_eq!(poll.tally().ours, [0, 2].into());
    assert_eq!(poll.next_vote_count(), 2);
    // A second vote waits beside the first, replaces the pending one
    // before it, and takes over once sent. Someone no longer registered
    // isn't waited for.
    poll.apply_vote(our_vote(&[1], 2, 20, &["a"]), VoteSource::ThisDevice);
    poll.apply_vote(our_vote(&[0], 3, 30, &["a", "gone"]), VoteSource::ThisDevice);
    assert_eq!(poll.votes.len(), 2);
    assert_eq!(poll.tally().ours, [0, 2].into());
    assert_eq!(poll.tally().pending, Some([0].into()));
    assert!(!poll.record_vote_sends(2, &names(&["a"]), &[]), "replaced");
    assert!(poll.record_vote_sends(3, &names(&["a"]), &names(&["gone"])));
    assert_eq!(poll.votes, vec![vote(None, &[0], 3, 30)]);
    // Given up having reached nobody: it goes.
    poll.apply_vote(our_vote(&[1], 4, 40, &["a", "b"]), VoteSource::ThisDevice);
    assert!(poll.give_up_vote(4));
    assert_eq!(poll.votes, vec![vote(None, &[0], 3, 30)]);
    // Given up having reached some: it counts, as Desktop's does.
    poll.apply_vote(our_vote(&[2], 5, 50, &["a", "b"]), VoteSource::ThisDevice);
    poll.record_vote_sends(5, &names(&["a"]), &[]);
    assert!(poll.give_up_vote(5));
    assert_eq!(poll.tally().ours, [2].into());
    assert_eq!(poll.our_pending_vote(), None);
    // One from before recipients were kept is addressed when resent.
    poll.apply_vote(vote(None, &[1], 6, 60), VoteSource::ThisDevice);
    assert!(poll.address_our_vote(6, &names(&["a"])));
    assert!(!poll.address_our_vote(6, &names(&["b"])), "only once");
    assert_eq!(poll.our_pending_vote().map(PollVote::unsent), Some(names(&["a"])));
}

#[test]
fn our_poll_counts_votes_only_from_who_it_reached() {
    let mut poll = lunch();
    assert_eq!(poll.reached("a"), None, "not known");
    assert!(poll.record_reached(names(&["a"])));
    assert!(!poll.record_reached(names(&["a"])));
    assert_eq!((poll.reached("a"), poll.reached("b")), (Some(true), Some(false)));
    // Kept with the poll.
    let json = serde_json::to_string(&poll).unwrap();
    assert_eq!(serde_json::from_str::<Poll>(&json).unwrap(), poll);
    assert!(!serde_json::to_string(&lunch()).unwrap().contains("reached"));
    assert_eq!(poll_ended_preview(Some("Ana"), "Lunch\ntoday?"), "📊 Ana ended the poll: Lunch today?");
    assert_eq!(poll_ended_preview(None, "Lunch?"), "📊 You ended the poll: Lunch?");
}

#[test]
fn poll_limits_follow_desktop() {
    let options = |n: usize| (0..n).map(|i| format!("Option {i}")).collect::<Vec<_>>();
    assert!(valid_poll("Q", &options(2)));
    assert!(!valid_poll("", &options(2)));
    assert!(!valid_poll("Q", &options(1)));
    assert!(!valid_poll("Q", &options(11)));
    assert!(!valid_poll("Q", &[String::new(), "B".into()]));
    // Graphemes, not chars or bytes: 200 accented letters (two chars each)
    // are fine, 201 aren't.
    assert!(valid_poll(&"e\u{301}".repeat(200), &options(2)));
    assert!(!valid_poll(&"e\u{301}".repeat(201), &options(2)));
    // And at most 2 KiB: 100 family emoji are 100 characters, 2,500 bytes.
    assert!(!valid_poll(&"👩‍👩‍👧‍👦".repeat(100), &options(2)));

    assert_eq!(poll_send_error("  ", &options(2)), Some("Poll question is required"));
    assert_eq!(poll_send_error(&"a".repeat(101), &options(2)), Some("Question is too long"));
    assert_eq!(poll_send_error("Q", &["A".into(), "  ".into()]), Some("Poll requires at least 2 options"));
    assert_eq!(poll_send_error("Q", &["A".into(), "a".repeat(101)]), Some("Option is too long"));
    assert_eq!(poll_send_error(" Q ", &["A".into(), "".into(), "B".into()]), None);

    assert_eq!(vote_options(&[2, 2, 0]), Some(vec![2, 0]));
    assert_eq!(vote_options(&[10]), None);
    assert_eq!(vote_options(&[]), Some(vec![]));
}

#[tokio::test]
async fn polls_are_stored_with_their_votes() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("g:aa", "bob", 10, "Lunch?")).await.unwrap();
    db.set_poll(id, &lunch()).await.unwrap();
    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("bob: 📊 Poll: Lunch?"));
    // Delivered again: the stored one (with its votes) stays.
    assert!(db.update_poll(id, |p| p.apply_vote(vote(Some("carol"), &[1], 1, 20), VoteSource::Other)).await.unwrap());
    db.set_poll(id, &lunch()).await.unwrap();
    let found = db.find_poll(Some("bob"), 10).await.unwrap().unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.poll.unwrap().tally().count(1), 1);
    assert!(db.find_poll(Some("carol"), 10).await.unwrap().is_none(), "by its author");
    assert!(!db.update_poll(id, |_| false).await.unwrap());
    // Search finds its question.
    assert_eq!(db.search_messages("lunch", 10).await.unwrap().len(), 1);
    // Deleted: no longer a poll.
    db.mark_deleted(id).await.unwrap();
    assert!(db.find_poll(Some("bob"), 10).await.unwrap().is_none());
    assert!(!db.update_poll(id, |p| p.end()).await.unwrap());
    // A plain message isn't one.
    let plain = db.insert_message(&incoming("g:aa", "bob", 30, "hi")).await.unwrap();
    assert!(!db.update_poll(plain, |p| p.end()).await.unwrap());
}

#[tokio::test]
async fn early_events_wait_for_their_message_and_expire_as_desktops_do() {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let db = test_db().await;
    let now = 100 * DAY;
    db.keep_early_event(Some("bob"), 10, EarlyKeep::Envelope, "delete", now).await.unwrap();
    db.keep_early_event(Some("bob"), 10, EarlyKeep::Envelope, "delete", now).await.unwrap();
    db.keep_early_event(Some("bob"), 10, EarlyKeep::Envelope, "react", now).await.unwrap();
    db.keep_early_event(None, 10, EarlyKeep::SyncTask, "receipt", now).await.unwrap();
    let actions = |events: Vec<EarlyEvent>| events.into_iter().map(|e| e.action).collect::<Vec<_>>();
    assert_eq!(actions(db.early_events_for(Some("bob"), 10).await.unwrap()), ["delete", "react"], "kept once, oldest first");
    assert_eq!(actions(db.early_events_for(None, 10).await.unwrap()), ["receipt"]);
    assert!(db.early_events_for(Some("bob"), 11).await.unwrap().is_empty());

    let delete = db.early_events_for(Some("bob"), 10).await.unwrap()[0].id;
    db.remove_early_event(delete).await.unwrap();
    assert_eq!(actions(db.early_events().await.unwrap()), ["react", "receipt"]);

    // Envelopes: read at 10 starts at most, and kept 45 days.
    for _ in 0..10 {
        assert_eq!(db.load_early_events(now).await.unwrap().len(), 2);
    }
    assert_eq!(actions(db.load_early_events(now).await.unwrap()), ["receipt"], "the reaction had its 10 tries");
    // Sync tasks: dropped once tried 5 times and 2 days old, not before.
    assert_eq!(db.load_early_events(now + 2 * DAY - 1).await.unwrap().len(), 1);
    assert!(db.load_early_events(now + 2 * DAY + 1).await.unwrap().is_empty());

    db.keep_early_event(Some("carol"), 5, EarlyKeep::Envelope, "edit", now).await.unwrap();
    assert_eq!(db.load_early_events(now + 45 * DAY).await.unwrap().len(), 1);
    assert!(db.load_early_events(now + 45 * DAY + 1).await.unwrap().is_empty(), "45 days at most");
}

fn photo(path: Option<&str>) -> AttachmentMeta {
    AttachmentMeta {
        content_type: "image/jpeg".into(),
        file_name: None,
        size: 10,
        width: None,
        height: None,
        local_path: path.map(Into::into),
        sticker: false,
        gif: false,
        voice: false,
        remote: None,
        thumbnail_path: None,
    }
}

#[tokio::test]
async fn view_once_messages_are_viewed_once_then_erased() {
    let db = test_db().await;
    let mut m = incoming("d:alice", "alice", 100, "");
    m.attachments = vec![photo(Some("/tmp/vo.jpg"))];
    let id = db.insert_message_view_once(&m, Some(ViewOnce::Ready)).await.unwrap();
    let got = db.message(id).await.unwrap().unwrap();
    assert_eq!(got.view_once, Some(ViewOnce::Ready));
    assert_eq!(got.thumbnail(), None, "never quoted with its photo");
    let chat = db.conversation(&conv("d:alice")).await.unwrap().unwrap();
    assert_eq!((chat.last_message_preview.as_deref(), chat.unread_count), (Some("📷 View-once Photo"), 1));
    assert!(db.media_messages(&conv("d:alice"), 10).await.unwrap().is_empty(), "not in All Media");
    // Delivered again: still the one, still ready.
    assert_eq!(db.insert_message_view_once(&m, Some(ViewOnce::Ready)).await.unwrap(), id);

    // Viewing erases it, reads it, and hands back its files.
    let erased = db.erase_view_once(id, ViewOnce::Viewed).await.unwrap().unwrap();
    assert_eq!(erased, ErasedViewOnce { conversation: conv("d:alice"), files: vec!["/tmp/vo.jpg".into()] });
    let got = db.message(id).await.unwrap().unwrap();
    assert_eq!((got.view_once, got.attachments.len()), (Some(ViewOnce::Viewed), 0));
    let chat = db.conversation(&conv("d:alice")).await.unwrap().unwrap();
    assert_eq!((chat.last_message_preview.as_deref(), chat.unread_count), (Some("View-once Media"), 0));
    // Only once; a download finishing late isn't kept.
    assert_eq!(db.erase_view_once(id, ViewOnce::Viewed).await.unwrap(), None);
    assert_eq!(db.erase_view_once(id, ViewOnce::Gone).await.unwrap(), None);
    assert!(!db.set_attachment_path(id, 0, "/tmp/late.jpg").await.unwrap());
    // Nor does a re-delivery bring it back.
    db.insert_message_view_once(&m, Some(ViewOnce::Ready)).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().view_once, Some(ViewOnce::Viewed));

    // Ours can't be "viewed", only erased.
    let mut ours = outgoing("d:alice", 200, "");
    ours.attachments = vec![photo(None)];
    let ours = db.insert_message_view_once(&ours, Some(ViewOnce::Ready)).await.unwrap();
    assert_eq!(db.erase_view_once(ours, ViewOnce::Viewed).await.unwrap(), None);
    assert!(db.erase_view_once(ours, ViewOnce::Gone).await.unwrap().is_some());
    // Ordinary messages aren't touched.
    let plain = db.insert_message(&incoming("d:alice", "alice", 300, "hi")).await.unwrap();
    assert_eq!(db.erase_view_once(plain, ViewOnce::Viewed).await.unwrap(), None);
    assert_eq!(db.message(plain).await.unwrap().unwrap().view_once, None);
}

#[tokio::test]
async fn unviewed_view_once_messages_go_after_thirty_days() {
    let db = test_db().await;
    let at = |sent_at: i64| {
        let mut m = incoming("d:alice", "alice", sent_at, "");
        m.attachments = vec![photo(None)];
        m
    };
    let old = db.insert_message_view_once(&at(1_000), Some(ViewOnce::Ready)).await.unwrap();
    let new = db.insert_message_view_once(&at(5_000), Some(ViewOnce::Ready)).await.unwrap();
    let viewed = db.insert_message_view_once(&at(500), Some(ViewOnce::Ready)).await.unwrap();
    db.erase_view_once(viewed, ViewOnce::Viewed).await.unwrap();
    // (received_at is sent_at + 5.)
    assert_eq!(db.oldest_view_once().await.unwrap(), Some(1_005));
    assert_eq!(db.view_once_to_erase(1_005).await.unwrap(), vec![old]);
    db.erase_view_once(old, ViewOnce::Gone).await.unwrap();
    assert_eq!(db.message(old).await.unwrap().unwrap().view_once, Some(ViewOnce::Gone));
    assert_eq!(db.oldest_view_once().await.unwrap(), Some(5_005));
    assert_eq!(db.view_once_to_erase(i64::MAX).await.unwrap(), vec![new]);
    // An unviewed one that went stays unread, as Desktop leaves it.
    assert_eq!(db.conversation(&conv("d:alice")).await.unwrap().unwrap().unread_count, 2);
}

#[tokio::test]
async fn votes_in_our_poll_are_unread_until_the_chat_is() {
    let db = test_db().await;
    let ours = db.insert_message(&outgoing("g:aa", 10, "Lunch?")).await.unwrap();
    db.set_poll(ours, &lunch()).await.unwrap();
    let chat = |db: &AppDb| {
        let db = db.clone();
        async move { db.conversation(&conv("g:aa")).await.unwrap().unwrap() }
    };
    assert!(!chat(&db).await.unread_poll_votes);
    // Our own changes don't make it unread; someone else's vote does,
    // without counting as an unread message (Desktop's isn't either).
    assert!(db.update_poll(ours, |p| p.apply_vote(vote(None, &[0], 1, 15), VoteSource::Sync)).await.unwrap());
    assert!(!chat(&db).await.unread_poll_votes);
    assert!(db.update_poll_unread(ours, |p| p.apply_vote(vote(Some("bob"), &[1], 1, 20), VoteSource::Other)).await.unwrap());
    let c = chat(&db).await;
    assert!(c.unread_poll_votes);
    assert_eq!(c.unread_count, 0);
    // A vote that doesn't apply leaves it as it was.
    db.take_unread_poll_votes(&conv("g:aa")).await.unwrap();
    assert!(!db.update_poll_unread(ours, |p| p.apply_vote(vote(Some("bob"), &[1], 1, 20), VoteSource::Other)).await.unwrap());
    assert!(!chat(&db).await.unread_poll_votes);
    // Reading the chat takes them, naming our poll for a read sync.
    assert!(db.update_poll_unread(ours, |p| p.apply_vote(vote(Some("bob"), &[2], 2, 30), VoteSource::Other)).await.unwrap());
    assert_eq!(db.take_unread_poll_votes(&conv("g:aa")).await.unwrap(), vec![10]);
    assert!(db.take_unread_poll_votes(&conv("g:aa")).await.unwrap().is_empty());
    assert!(!chat(&db).await.unread_poll_votes);
    // Or a read sync from another device naming it.
    assert!(db.update_poll_unread(ours, |p| p.apply_vote(vote(Some("bob"), &[0], 3, 40), VoteSource::Other)).await.unwrap());
    assert_eq!(db.mark_poll_votes_read(10).await.unwrap(), Some(conv("g:aa")));
    assert_eq!(db.mark_poll_votes_read(10).await.unwrap(), None);
    // Deleted: nothing left to read.
    assert!(db.update_poll_unread(ours, |p| p.apply_vote(vote(Some("bob"), &[1], 4, 50), VoteSource::Other)).await.unwrap());
    db.mark_deleted(ours).await.unwrap();
    assert!(!chat(&db).await.unread_poll_votes);
}

#[tokio::test]
async fn a_polls_end_is_the_preview_and_someone_elses_is_unread() {
    let db = test_db().await;
    db.upsert_conversation(&conv("d:bob"), ConversationUpdate { title: Some("Bob Builder".into()), ..Default::default() })
        .await
        .unwrap();
    let poll = db.insert_message(&incoming("g:aa", "bob", 10, "Lunch?")).await.unwrap();
    db.set_poll(poll, &lunch()).await.unwrap();
    db.mark_read(&conv("g:aa")).await.unwrap();
    let ended = Notice::PollEnded { by: Some("bob".into()), question: "Lunch?".into(), sent_at: 10 };
    db.insert_unread_notice(&conv("g:aa"), 50, &ended).await.unwrap().unwrap();
    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("📊 Bob Builder ended the poll: Lunch?"));
    assert_eq!((c.last_message_at, c.unread_count), (50, 1));
    // A repeat adds nothing.
    assert_eq!(db.insert_unread_notice(&conv("g:aa"), 50, &ended).await.unwrap(), None);
    assert_eq!(db.conversation(&conv("g:aa")).await.unwrap().unwrap().unread_count, 1);
    // Reading names who ended it and when, for receipts and read syncs.
    assert_eq!(db.mark_read(&conv("g:aa")).await.unwrap(), vec![("bob".into(), 50)]);
    // A read sync from another device can name it too.
    db.insert_unread_notice(&conv("g:aa"), 60, &ended).await.unwrap().unwrap();
    assert_eq!(db.conversation(&conv("g:aa")).await.unwrap().unwrap().unread_count, 1);
    assert_eq!(db.apply_read_sync("bob", 60).await.unwrap(), ReadSync::Read(conv("g:aa")));
    assert_eq!(db.conversation(&conv("g:aa")).await.unwrap().unwrap().unread_count, 0);
    // Ours is read already, and is the preview too; other notices aren't.
    let mine = Notice::PollEnded { by: None, question: "Lunch?".into(), sent_at: 10 };
    db.insert_notice(&conv("g:aa"), 70, &mine).await.unwrap();
    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!((c.last_message_preview.as_deref(), c.unread_count), (Some("📊 You ended the poll: Lunch?"), 0));
    db.insert_notice(&conv("g:aa"), 80, &Notice::Timer { by: None, seconds: 60 }).await.unwrap();
    assert_eq!(db.conversation(&conv("g:aa")).await.unwrap().unwrap().last_message_at, 70);
}

#[tokio::test]
async fn pending_votes_and_unread_votes_are_found_by_index() {
    let db = test_db().await;
    let poll = db.insert_message(&incoming("g:aa", "bob", 10, "Lunch?")).await.unwrap();
    db.set_poll(poll, &lunch()).await.unwrap();
    db.insert_message(&incoming("g:aa", "bob", 20, "hi")).await.unwrap();
    assert!(db.polls_with_pending_votes().await.unwrap().is_empty());
    let pending = PollVote { sends: [("bob".to_owned(), false)].into(), ..vote(None, &[1], 1, 30) };
    db.update_poll(poll, |p| p.apply_vote(pending, VoteSource::ThisDevice)).await.unwrap();
    let found = db.polls_with_pending_votes().await.unwrap();
    assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), [poll]);
    db.update_poll(poll, |p| p.record_vote_sends(1, &["bob".into()], &[])).await.unwrap();
    assert!(db.polls_with_pending_votes().await.unwrap().is_empty());

    let plan = |sql: String| {
        let db = db.clone();
        async move {
            let rows: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}"))).fetch_all(&db.pool).await.unwrap();
            rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ")
        }
    };
    let list = plan(format!("SELECT {CONVERSATION_COLUMNS} FROM conversations")).await;
    assert!(list.contains("messages_unread_poll_votes"), "{list}");
    let sync =
        plan("UPDATE messages SET unread_poll_votes = 0 WHERE author_key = '' AND sent_at = 1 AND unread_poll_votes = 1".into()).await;
    assert!(sync.contains("messages_outgoing_sent_at"), "{sync}");
}

#[test]
fn locators_saved_before_the_backup_tier_still_read() {
    // As the previous version wrote them: a transit key, no media tier.
    let old = r#"{"cdn":3,"cdn_key":"abc","key":"00","digest":"11"}"#;
    let remote: RemoteAttachment = serde_json::from_str(old).unwrap();
    assert_eq!((remote.cdn_key.as_deref(), remote.media_tier_cdn), (Some("abc"), None));
    let old = r#"{"content_type":"image/png","file_name":null,"size":1,"width":null,"height":null,"local_path":null}"#;
    let attachment: AttachmentMeta = serde_json::from_str(old).unwrap();
    assert_eq!(attachment.thumbnail_path, None);
    // Only in the backup tier: no transit key written.
    let backup_only = RemoteAttachment { cdn_key: None, media_tier_cdn: Some(3), ..remote };
    let json = serde_json::to_string(&backup_only).unwrap();
    assert!(!json.contains("cdn_key"), "{json}");
    assert_eq!(serde_json::from_str::<RemoteAttachment>(&json).unwrap(), backup_only);
}

#[tokio::test]
async fn direct_chats_are_message_requests_until_trusted() {
    let db = test_db().await;
    let alice = conv("d:alice");
    // No messages yet: not a request (Desktop's empty 1:1 chat).
    db.upsert_conversation(&alice, ConversationUpdate::default()).await.unwrap();
    assert!(db.conversation(&alice).await.unwrap().unwrap().accepted);
    // Their message makes it one.
    db.insert_message(&incoming("d:alice", "alice", 10, "hi")).await.unwrap();
    let chat = db.conversation(&alice).await.unwrap().unwrap();
    assert!(!chat.accepted);
    assert_eq!(chat.message_request, MessageRequestResponse::Unknown);
    // A notice doesn't count as ours.
    db.insert_notice(&alice, 11, &Notice::Timer { by: None, seconds: 60 }).await.unwrap();
    assert!(!db.conversation(&alice).await.unwrap().unwrap().accepted);
    // Each makes it accepted: an address book entry, profile sharing,
    // a message of ours, or our answer.
    db.replace_system_contacts(&["alice".into()]).await.unwrap();
    let chat = db.conversation(&alice).await.unwrap().unwrap();
    assert!(chat.accepted && chat.system_contact);
    assert!(!db.replace_system_contacts(&["alice".into()]).await.unwrap(), "unchanged");
    db.replace_system_contacts(&[]).await.unwrap();
    assert!(!db.conversation(&alice).await.unwrap().unwrap().accepted);
    db.set_profile_sharing(&alice, true).await.unwrap();
    assert!(db.conversation(&alice).await.unwrap().unwrap().accepted);
    db.set_profile_sharing(&alice, false).await.unwrap();
    db.set_message_request(&alice, MessageRequestResponse::Accept).await.unwrap();
    let chat = db.conversation(&alice).await.unwrap().unwrap();
    assert!(chat.accepted);
    assert_eq!(chat.message_request, MessageRequestResponse::Accept);
    db.set_message_request(&alice, MessageRequestResponse::Block).await.unwrap();
    assert!(!db.conversation(&alice).await.unwrap().unwrap().accepted);
    db.insert_message(&outgoing("d:alice", 20, "hello")).await.unwrap();
    assert!(db.conversation(&alice).await.unwrap().unwrap().accepted);
    // Note to Self and the Signal chat always are.
    db.upsert_conversation(&conv("d:me"), ConversationUpdate { kind: Some(ConversationKind::NoteToSelf), ..Default::default() })
        .await
        .unwrap();
    db.insert_message(&incoming(&ConversationId::signal().0, SIGNAL_ACI, 5, "news")).await.unwrap();
    assert!(db.conversation(&conv("d:me")).await.unwrap().unwrap().accepted);
    assert!(db.conversation(&ConversationId::signal()).await.unwrap().unwrap().accepted);
}

#[tokio::test]
async fn groups_are_accepted_when_someone_trusted_added_us() {
    let db = test_db().await;
    let group = conv("g:aa");
    db.upsert_conversation(&group, ConversationUpdate { kind: Some(ConversationKind::Group), ..Default::default() }).await.unwrap();
    // Desktop: a group nobody trusted added us to is a request, messages or not.
    assert!(!db.conversation(&group).await.unwrap().unwrap().accepted);
    db.upsert_conversation(&group, ConversationUpdate { added_by: Some("bob".into()), ..Default::default() }).await.unwrap();
    assert!(!db.conversation(&group).await.unwrap().unwrap().accepted);
    db.set_profile_sharing(&conv("d:bob"), true).await.unwrap();
    let chat = db.conversation(&group).await.unwrap().unwrap();
    assert!(chat.accepted);
    assert_eq!(chat.added_by.as_deref(), Some("bob"));
    // Not an invitation, though, until we take it; a request to join is.
    let state = |membership| GroupState { membership, ..Default::default() };
    let invited = state(Membership::Invited { by: Some("carol".into()) });
    db.upsert_conversation(&group, ConversationUpdate { group_state: Some(invited), ..Default::default() }).await.unwrap();
    let chat = db.conversation(&group).await.unwrap().unwrap();
    assert!(chat.invited() && !chat.accepted);
    assert_eq!(chat.inviter(), Some("carol"));
    let requesting = state(Membership::Requesting { password: String::new() });
    db.upsert_conversation(&group, ConversationUpdate { group_state: Some(requesting), ..Default::default() }).await.unwrap();
    let chat = db.conversation(&group).await.unwrap().unwrap();
    assert!(chat.accepted && chat.inviter() == Some("bob"));
    db.upsert_conversation(&group, ConversationUpdate { group_state: Some(state(Membership::Invited { by: None })), ..Default::default() })
        .await
        .unwrap();
    assert!(!db.conversation(&group).await.unwrap().unwrap().accepted);
    db.set_message_request(&group, MessageRequestResponse::Accept).await.unwrap();
    assert!(db.conversation(&group).await.unwrap().unwrap().accepted);
    // Our invitation is the chat's preview, naming who sent it.
    db.replace_contact_names(&[("bob".into(), "Bob Stone".into())]).await.unwrap();
    let invited = Notice::Group { by: Some("bob".into()), change: GroupNotice::Invited { member: None } };
    db.insert_notice(&group, 50, &invited).await.unwrap();
    let chat = db.conversation(&group).await.unwrap().unwrap();
    assert_eq!((chat.last_message_preview.as_deref(), chat.last_message_at), (Some("Bob Stone invited you to the group."), 50));
    // Someone else's invitation isn't.
    let other_invite = Notice::Group { by: Some("bob".into()), change: GroupNotice::Invited { member: Some("carol".into()) } };
    db.insert_notice(&group, 60, &other_invite).await.unwrap();
    assert_eq!(db.conversation(&group).await.unwrap().unwrap().last_message_at, 50);
    // Group profile sharing (Storage Service's `whitelisted`) counts too.
    let other = conv("g:bb");
    db.insert_message(&incoming("g:bb", "carol", 1, "hi")).await.unwrap();
    assert!(!db.conversation(&other).await.unwrap().unwrap().accepted);
    db.set_profile_sharing(&other, true).await.unwrap();
    assert!(db.conversation(&other).await.unwrap().unwrap().accepted);
}

#[tokio::test]
async fn spam_reports_name_the_newest_incoming_server_guids() {
    let db = test_db().await;
    let chat = conv("g:aa");
    for (at, author, guid) in [(1, "bob", "g1"), (2, "bob", "g2"), (3, "carol", "g3"), (4, "bob", "g4"), (5, "bob", "g5")] {
        db.insert_received(&incoming("g:aa", author, at, "spam"), None, Some(guid)).await.unwrap();
    }
    // A re-delivery keeps the first.
    db.insert_received(&incoming("g:aa", "bob", 5, "spam"), None, Some("other")).await.unwrap();
    // Ours never have one.
    db.insert_received(&outgoing("g:aa", 6, "no"), None, Some("mine")).await.unwrap();
    db.insert_message(&incoming("g:aa", "bob", 7, "no guid")).await.unwrap();
    assert_eq!(db.server_guids_for_spam(&chat, None).await.unwrap(), ["g5", "g4", "g3"]);
    assert_eq!(db.server_guids_for_spam(&chat, Some("bob")).await.unwrap(), ["g5", "g4", "g2"]);
    assert!(db.server_guids_for_spam(&conv("d:x"), None).await.unwrap().is_empty());

    assert_eq!(db.reporting_token("bob").await.unwrap(), None);
    assert!(db.set_reporting_token("bob", "dG9rZW4=").await.unwrap());
    assert!(!db.set_reporting_token("bob", "dG9rZW4=").await.unwrap());
    assert_eq!(db.reporting_token("bob").await.unwrap().as_deref(), Some("dG9rZW4="));
    // No chat for them is made.
    let bob = conv("d:bob");
    assert!(db.conversation(&bob).await.unwrap().is_none());
    db.upsert_conversation(&bob, ConversationUpdate::default()).await.unwrap();
    db.set_reported(&bob).await.unwrap();
    assert!(db.conversation(&bob).await.unwrap().unwrap().reported);

    // Clearing a blocked request keeps the chat and its block.
    db.insert_message(&incoming("d:bob", "bob", 1, "spam")).await.unwrap();
    db.set_blocked(&bob, true).await.unwrap();
    db.clear_messages(&bob).await.unwrap();
    let chat = db.conversation(&bob).await.unwrap().unwrap();
    assert!(chat.blocked && chat.unread_count == 0 && chat.last_message_preview.is_none());
    assert!(db.messages(&bob, None, 10).await.unwrap().is_empty());
}

#[test]
fn message_request_answers_round_trip_the_wire_numbers() {
    for n in 0..=6 {
        assert_eq!(MessageRequestResponse::from_wire(n).to_wire(), n);
    }
    assert_eq!(MessageRequestResponse::from_wire(99), MessageRequestResponse::Unknown);
    assert!(MessageRequestResponse::BlockAndSpam.blocks() && MessageRequestResponse::BlockAndSpam.spam());
    assert!(MessageRequestResponse::BlockAndDelete.deletes() && !MessageRequestResponse::Accept.blocks());
    // The Signal chat's id is spelled out in the accepted SQL.
    assert!(CONVERSATION_COLUMNS.contains(&format!("'d:{SIGNAL_ACI}'")));
}

#[tokio::test]
async fn whether_a_chat_is_accepted_is_found_by_index() {
    let db = test_db().await;
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as(AssertSqlSafe(format!("EXPLAIN QUERY PLAN SELECT {CONVERSATION_COLUMNS} FROM conversations")))
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let plan = rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ");
    assert!(!plan.contains("SCAN m"), "{plan}");
    assert!(!plan.contains("SCAN s"), "{plan}");
    assert!(!plan.contains("SCAN p"), "{plan}");
}

#[tokio::test]
async fn send_states_move_forward_and_set_the_bubble() {
    let db = test_db().await;
    let id = db.insert_message(&outgoing("g:aa", 100, "hi")).await.unwrap();
    let failed = |e: &str| Some(e.to_owned());
    let status = db
        .record_send_states(id, &[("a".into(), SendStatus::Sent, None), ("b".into(), SendStatus::Failed, failed(SEND_ERROR_IDENTITY))], 110)
        .await
        .unwrap();
    // Some failed: partly sent.
    assert_eq!(status, Some(DeliveryStatus::PartialSent));
    assert_eq!(db.message(id).await.unwrap().unwrap().status, DeliveryStatus::PartialSent);
    assert_eq!(db.failed_recipients(id).await.unwrap(), ["b"]);
    // A receipt from someone it went to moves them on; from anyone else, nothing.
    assert_eq!(db.apply_receipt_from("a", 100, SendStatus::Read, 120).await.unwrap(), [(conv("g:aa"), id)]);
    assert_eq!(db.apply_receipt_from("c", 100, SendStatus::Read, 120).await.unwrap(), []);
    // Never back.
    assert_eq!(db.apply_receipt_from("a", 100, SendStatus::Delivered, 130).await.unwrap(), []);
    // The resend: pending, then sent.
    db.record_send_states(id, &[("b".into(), SendStatus::Pending, None)], 140).await.unwrap();
    let status = db.record_send_states(id, &[("b".into(), SendStatus::Sent, None)], 150).await.unwrap();
    assert_eq!(status, Some(DeliveryStatus::Read), "the furthest any got");
    let states = db.send_states(id).await.unwrap();
    assert_eq!(states.iter().map(|s| (s.recipient.as_str(), s.status, s.updated_at, s.error.is_some())).collect::<Vec<_>>(), [
        ("a", SendStatus::Read, Some(120), false),
        ("b", SendStatus::Sent, Some(150), false)
    ]);
    // A message without states takes a receipt as before.
    let old = db.insert_message(&outgoing("d:x", 200, "old")).await.unwrap();
    db.update_status(&conv("d:x"), 200, DeliveryStatus::Sent).await.unwrap();
    assert_eq!(db.apply_receipt_from("x", 200, SendStatus::Viewed, 210).await.unwrap(), [(conv("d:x"), old)]);
    assert_eq!(db.message(old).await.unwrap().unwrap().status, DeliveryStatus::Read);
    // Deleting the message drops its states.
    db.remove_message(id).await.unwrap();
    assert!(db.send_states(id).await.unwrap().is_empty());
}

#[test]
fn the_bubble_reads_its_recipients_as_desktop() {
    let s = |status| SendState { recipient: String::new(), status, updated_at: None, error: None };
    assert_eq!(summarize(&[]), None);
    assert_eq!(summarize(&[s(SendStatus::Failed)]), Some(DeliveryStatus::Failed));
    assert_eq!(summarize(&[s(SendStatus::Pending), s(SendStatus::Failed)]), Some(DeliveryStatus::Failed));
    assert_eq!(summarize(&[s(SendStatus::Delivered), s(SendStatus::Sent)]), Some(DeliveryStatus::Delivered));
    assert_eq!(summarize(&[s(SendStatus::Viewed), s(SendStatus::Pending)]), Some(DeliveryStatus::Viewed));
    assert_eq!(summarize(&[s(SendStatus::Pending)]), Some(DeliveryStatus::Sending));
    // Desktop's order puts skipped past read.
    assert_eq!(summarize(&[s(SendStatus::Skipped), s(SendStatus::Sent)]), Some(DeliveryStatus::Read));
    assert_eq!(SendStatus::Pending.advance(SendStatus::Failed), SendStatus::Failed);
    assert_eq!(SendStatus::Sent.advance(SendStatus::Failed), SendStatus::Sent);
    assert_eq!(SendStatus::Failed.advance(SendStatus::Pending), SendStatus::Pending);
}

#[tokio::test]
async fn settings_are_read_by_prefix() {
    let db = test_db().await;
    db.set_setting("verified.a", "k1").await.unwrap();
    db.set_setting("verified.b", "").await.unwrap();
    db.set_setting("verifiedz", "x").await.unwrap();
    db.set_setting("identity.a", "k2").await.unwrap();
    let mut verified = db.settings_with_prefix("verified.").await.unwrap();
    verified.sort();
    assert_eq!(verified, [("a".to_owned(), "k1".to_owned()), ("b".to_owned(), String::new())]);
}

#[tokio::test]
async fn a_new_number_is_noted_only_when_it_replaces_one() {
    let db = test_db().await;
    let phone = |p: &str| ConversationUpdate { phone: Some(p.into()), ..Default::default() };
    assert_eq!(db.upsert_conversation_noting_phone(&conv("d:a"), phone("+15550100")).await.unwrap(), None);
    assert_eq!(db.upsert_conversation_noting_phone(&conv("d:a"), phone("+15550100")).await.unwrap(), None);
    // No number given keeps the one there.
    assert_eq!(db.upsert_conversation_noting_phone(&conv("d:a"), ConversationUpdate::default()).await.unwrap(), None);
    assert_eq!(db.upsert_conversation_noting_phone(&conv("d:a"), phone("+15550199")).await.unwrap(), Some("+15550100".into()));
    assert_eq!(db.conversation(&conv("d:a")).await.unwrap().unwrap().phone.as_deref(), Some("+15550199"));
}

#[tokio::test]
async fn a_profile_name_change_is_noted_only_from_a_name_they_had() {
    let db = test_db().await;
    let named = |given: &str, family: &str| FetchedProfile { given_name: given.into(), family_name: family.into(), ..Default::default() };
    // A username alone gives no name to change from.
    db.replace_usernames(&[("a".into(), "al.01".into())]).await.unwrap();
    assert_eq!(db.save_fetched_profile_noting_name("a", &named("Al", ""), 10).await.unwrap(), (true, None));
    assert_eq!(
        db.save_fetched_profile_noting_name("a", &named("Al", "Bo"), 20).await.unwrap(),
        (true, Some(("Al".into(), "Al Bo".into())))
    );
    // The same combined name, split differently, isn't a change.
    assert_eq!(db.save_fetched_profile_noting_name("a", &named("Al Bo", ""), 30).await.unwrap(), (true, None));
    // Removing the name is.
    assert_eq!(db.save_fetched_profile_noting_name("a", &named("", ""), 40).await.unwrap(), (true, Some(("Al Bo".into(), String::new()))));
}

#[tokio::test]
async fn profiles_keep_fetches_and_usernames_apart() {
    let db = test_db().await;
    let fetched = FetchedProfile {
        given_name: "Maya".into(),
        family_name: "Chen".into(),
        about: "Out hiking".into(),
        about_emoji: "🥾".into(),
        badges: vec![ProfileBadge { id: "R_LOW".into(), expires_at: None, visible: None }],
    };
    assert!(db.save_fetched_profile("a", &fetched, 10).await.unwrap());
    // The same again only moves the time.
    assert!(!db.save_fetched_profile("a", &fetched, 20).await.unwrap());
    db.replace_usernames(&[("a".into(), "maya.42".into()), ("b".into(), "bo.01".into())]).await.unwrap();
    let a = db.person_profile("a").await.unwrap().unwrap();
    assert_eq!((a.profile_name().as_deref(), a.about.as_str(), a.fetched_at), (Some("Maya Chen"), "Out hiking", 20));
    assert_eq!(a.username.as_deref(), Some("maya.42"));
    // A fetch leaves the username; a new sync replaces them all.
    db.save_fetched_profile("a", &FetchedProfile { about: String::new(), ..fetched.clone() }, 30).await.unwrap();
    db.replace_usernames(&[("b".into(), "bo.02".into())]).await.unwrap();
    let a = db.person_profile("a").await.unwrap().unwrap();
    assert_eq!((a.about.as_str(), a.username), ("", None));
    let b = db.person_profile("b").await.unwrap().unwrap();
    assert_eq!((b.fetched_at, b.username.as_deref()), (0, Some("bo.02")));
    assert_eq!(db.person_profile("c").await.unwrap(), None);
    assert_eq!(db.badge_holders().await.unwrap().get("a").cloned(), Some(vec!["R_LOW".to_owned()]));

    // Our own name, from the account record.
    assert!(db.set_profile_name("me", "Sam", "").await.unwrap());
    assert!(!db.set_profile_name("me", "Sam", "").await.unwrap());
    assert_eq!(db.person_profile("me").await.unwrap().unwrap().given_name, "Sam");
}

#[tokio::test]
async fn badges_and_their_images_are_kept() {
    let db = test_db().await;
    let badge = Badge {
        id: "R_LOW".into(),
        category: BadgeCategory::Donor,
        name: "Signal Sustainer".into(),
        description: "{short_name} supports Signal".into(),
        images: vec![BadgeImage { light: Some("https://u/l".into()), ..Default::default() }],
    };
    db.save_badges(std::slice::from_ref(&badge)).await.unwrap();
    db.save_badges(&[Badge { name: "Sustainer".into(), ..badge.clone() }]).await.unwrap();
    let badges = db.badges().await.unwrap();
    assert_eq!(badges.len(), 1);
    assert_eq!((badges[0].name.as_str(), badges[0].category), ("Sustainer", BadgeCategory::Donor));
    assert_eq!(badges[0].images, badge.images);
    db.set_badge_image_file("https://u/l", "/tmp/l.svg").await.unwrap();
    assert_eq!(db.badge_image_files().await.unwrap().get("https://u/l").map(String::as_str), Some("/tmp/l.svg"));
}

#[tokio::test]
async fn user_messages_are_told_from_notices() {
    let db = test_db().await;
    let chat = conv("d:new");
    db.upsert_conversation(&chat, ConversationUpdate::default()).await.unwrap();
    assert!(!db.has_user_messages(&chat).await.unwrap());
    // A notice (a timer change, say) isn't anyone writing.
    db.insert_notice(&chat, 10, &Notice::Timer { by: None, seconds: 60 }).await.unwrap();
    assert!(!db.has_user_messages(&chat).await.unwrap());
    db.insert_message(&incoming("d:new", "alice", 20, "hi")).await.unwrap();
    assert!(db.has_user_messages(&chat).await.unwrap());
}

#[tokio::test]
async fn unread_reminders_find_who_wrote_mentioned_and_replied() {
    let db = test_db().await;
    let chat = conv("g:aa");
    let quote = |author: Option<&str>| Quote {
        author: author.map(Into::into),
        author_name: String::new(),
        sent_at: 1,
        text: "q".into(),
        thumbnail: None,
        attachment: None,
        story: false,
    };
    // received_at is sent_at + 5.
    db.insert_message(&incoming("g:aa", "alice", 100, "hi")).await.unwrap();
    let mention = db.insert_message(&incoming("g:aa", "bob", 200, "@me")).await.unwrap();
    db.set_mentions(mention, &[MentionSpan { start: 0, len: 1, aci: "me".into() }]).await.unwrap();
    db.insert_message(&NewMessage { quote: Some(quote(None)), ..incoming("g:aa", "carol", 300, "re: yours") }).await.unwrap();
    db.insert_message(&NewMessage { quote: Some(quote(Some("alice"))), ..incoming("g:aa", "dave", 400, "re: alice") }).await.unwrap();
    db.insert_message(&outgoing("g:aa", 500, "mine")).await.unwrap();
    db.insert_notice(&chat, 600, &Notice::Timer { by: None, seconds: 60 }).await.unwrap();

    let ranges = db.unreminded_unread_ranges(&[(chat.clone(), 0), (conv("d:none"), 0)]).await.unwrap();
    assert_eq!(ranges, vec![UnreadRange { conversation: chat.clone(), oldest_unreminded: 105, newest_unread: 405 }]);
    // Since a reminder at 250, only what came after.
    let ranges = db.unreminded_unread_ranges(&[(chat.clone(), 250)]).await.unwrap();
    assert_eq!((ranges[0].oldest_unreminded, ranges[0].newest_unread), (305, 405));

    let data = db.unread_reminder_data(&chat, "me").await.unwrap();
    assert_eq!((data.unread, data.mentions, data.replies), (4, 1, 1));
    assert_eq!(data.senders, ["dave", "carol", "bob", "alice"]);
    assert_eq!((data.mentioners, data.repliers), (vec!["bob".to_owned()], vec!["carol".to_owned()]));
}

#[tokio::test]
async fn sealed_sender_is_kept_per_message() {
    let db = test_db().await;
    let id = db.insert_message(&incoming("d:a", "alice", 10, "hi")).await.unwrap();
    assert!(db.message(id).await.unwrap().unwrap().sealed_sender.is_empty());
    db.set_sealed_sender(id, &["alice".to_owned()]).await.unwrap();
    assert_eq!(db.message(id).await.unwrap().unwrap().sealed_sender, ["alice"]);
}

#[tokio::test]
async fn message_content_previews_and_gift_badge_states() {
    let db = test_db().await;
    let gift = |state| MessageContent::GiftBadge(GiftBadge { level: 100, expiration: 9_000, state, badge_id: None });
    let preview = |db: AppDb| async move {
        db.conversations().await.unwrap().into_iter().find(|c| c.id == conv("d:a")).and_then(|c| c.last_message_preview)
    };

    let theirs = db.insert_message(&incoming("d:a", "a", 1_000, "")).await.unwrap();
    db.set_content(theirs, &gift(GiftBadgeState::Unopened)).await.unwrap();
    assert_eq!(preview(db.clone()).await.as_deref(), Some("✨ a Name donated for you"));
    // A re-delivery doesn't take it back to unopened.
    assert!(db.gift_badge_viewed(theirs, true).await.unwrap());
    db.set_content(theirs, &gift(GiftBadgeState::Unopened)).await.unwrap();
    let message = db.message(theirs).await.unwrap().unwrap();
    assert_eq!(message.content, Some(gift(GiftBadgeState::Redeemed)));
    assert_eq!(preview(db.clone()).await.as_deref(), Some("✨ You redeemed a donation"));
    assert!(!db.gift_badge_viewed(theirs, true).await.unwrap(), "already redeemed");
    assert!(db.set_gift_badge_id(theirs, "BOOST").await.unwrap());

    // Not a gift badge: nothing to view.
    let contact = db.insert_message(&incoming("d:a", "a", 2_000, "")).await.unwrap();
    let shared = SharedContact {
        given_name: "Rosa".into(),
        avatar: Some(AttachmentMeta {
            content_type: "image/jpeg".into(),
            file_name: None,
            size: 10,
            width: None,
            height: None,
            local_path: None,
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        }),
        ..Default::default()
    };
    db.set_content(contact, &MessageContent::Contact(Box::new(shared))).await.unwrap();
    assert!(!db.gift_badge_viewed(contact, true).await.unwrap());
    assert_eq!(preview(db.clone()).await.as_deref(), Some("👤 Rosa"));
    assert!(db.set_contact_avatar_path(contact, "/tmp/rosa.jpg").await.unwrap());
    let Some(MessageContent::Contact(saved)) = db.message(contact).await.unwrap().unwrap().content else { panic!() };
    assert_eq!(saved.avatar.unwrap().local_path.as_deref(), Some("/tmp/rosa.jpg"));

    let paid = db.insert_message(&outgoing("d:a", 3_000, "")).await.unwrap();
    db.set_content(paid, &MessageContent::Payment(PaymentEvent::Activated)).await.unwrap();
    assert_eq!(preview(db.clone()).await.as_deref(), Some("💳 You activated Payments."));
}

#[tokio::test]
async fn deleting_a_message_clears_its_content() {
    let db = test_db().await;
    let contact = || MessageContent::Contact(Box::new(SharedContact { given_name: "Rosa".into(), ..Default::default() }));

    let mine = db.insert_message(&incoming("d:a", "a", 1_000, "")).await.unwrap();
    db.set_content(mine, &contact()).await.unwrap();
    db.mark_deleted(mine).await.unwrap();
    assert_eq!(db.message(mine).await.unwrap().unwrap().content, None);
    // A re-delivery after the delete doesn't bring it back.
    db.set_content(mine, &contact()).await.unwrap();
    assert_eq!(db.message(mine).await.unwrap().unwrap().content, None);

    let admin = db.insert_message(&incoming("d:a", "a", 2_000, "")).await.unwrap();
    db.set_content(admin, &contact()).await.unwrap();
    assert!(db.mark_deleted_by_admin(admin, &DeletedByAdmin { by: None }).await.unwrap());
    assert_eq!(db.message(admin).await.unwrap().unwrap().content, None);
}

#[test]
fn payment_descriptions_are_desktops() {
    let note = PaymentEvent::Notification { note: None };
    assert_eq!(note.description(Some("Sam"), Some("Sam")), "Sam started a payment to you");
    assert_eq!(note.description(None, Some("Sam")), "You started a payment to Sam");
    assert_eq!(PaymentEvent::ActivationRequest.description(None, None), "You sent a request to activate Payments.");
    assert_eq!(PaymentEvent::Activated.description(Some("Sam"), None), "Sam can now accept Payments.");
    assert_eq!(content_preview(&MessageContent::Payment(note), Some("Sam"), None), "💳 Payment");
}

#[tokio::test]
async fn chat_colors_change_as_desktops_picker_changes_them() {
    let db = test_db().await;
    assert_eq!(db.chat_colors().await.unwrap(), ChatColors::default());
    assert_eq!(db.chat_colors().await.unwrap().of(&conv("d:a")), &ChatColor::preset("ultramarine"));

    let blue = CustomColor { start: HueSaturation { hue: 200., saturation: 50., lightness: None }, end: None, deg: None };
    assert!(db.edit_chat_colors(ChatColorEdit::Default(ChatColor::preset("crimson"))).await.unwrap());
    assert!(!db.edit_chat_colors(ChatColorEdit::Default(ChatColor::preset("crimson"))).await.unwrap(), "no change");
    db.edit_chat_colors(ChatColorEdit::AddCustom { id: "c1".into(), color: blue, chat: Some(conv("d:a")) }).await.unwrap();
    db.edit_chat_colors(ChatColorEdit::Chat(conv("g:b"), Some(ChatColor::preset("sea")))).await.unwrap();
    let colors = db.chat_colors().await.unwrap();
    assert_eq!(colors.default, ChatColor::preset("crimson"));
    assert_eq!(colors.of(&conv("d:a")), &ChatColor::Custom { id: "c1".into(), value: blue });
    assert_eq!(colors.of(&conv("d:c")), &ChatColor::preset("crimson"));
    assert_eq!(colors.chats_with("c1"), 1);

    // Editing a custom color changes it, not the copy a chat keeps (as
    // Desktop's `editCustomColor`); the picker then chooses it again.
    let green = CustomColor { start: HueSaturation { hue: 120., ..blue.start }, ..blue };
    db.edit_chat_colors(ChatColorEdit::EditCustom { id: "c1".into(), color: green }).await.unwrap();
    let colors = db.chat_colors().await.unwrap();
    assert_eq!(colors.custom.get("c1"), Some(green));
    assert_eq!(colors.of(&conv("d:a")), &ChatColor::Custom { id: "c1".into(), value: blue });

    // Removing it takes it from every chat and resets the default, as
    // Desktop's `removeCustomColor` does.
    db.edit_chat_colors(ChatColorEdit::RemoveCustom { id: "c1".into() }).await.unwrap();
    let colors = db.chat_colors().await.unwrap();
    assert!(colors.custom.colors.is_empty());
    assert_eq!(colors.of(&conv("d:a")), &ChatColor::default());
    assert_eq!(colors.of(&conv("g:b")), &ChatColor::preset("sea"));

    db.edit_chat_colors(ChatColorEdit::ResetChats).await.unwrap();
    assert!(db.chat_colors().await.unwrap().chats.is_empty());
    db.edit_chat_colors(ChatColorEdit::Chat(conv("g:b"), Some(ChatColor::preset("sea")))).await.unwrap();
    db.edit_chat_colors(ChatColorEdit::Chat(conv("g:b"), None)).await.unwrap();
    assert!(db.chat_colors().await.unwrap().chats.is_empty());
}

#[tokio::test]
async fn chat_folders_change_as_desktops_do() {
    let db = test_db().await;
    let ids = |folders: &[ChatFolder]| current_folders(folders).into_iter().map(|f| f.id).collect::<Vec<_>>();
    let custom = |name: &str| ChatFolderParams { name: name.into(), ..ChatFolderParams::groups() };

    db.edit_chat_folders(ChatFolderEdit::Create { id: "a".into(), params: custom("Hikers") }).await.unwrap();
    // The All folder goes first, the others along one.
    assert_eq!(db.edit_chat_folders(ChatFolderEdit::EnsureAll { id: "all".into() }).await.unwrap().len(), 2);
    assert!(db.edit_chat_folders(ChatFolderEdit::EnsureAll { id: "all2".into() }).await.unwrap().is_empty());
    db.edit_chat_folders(ChatFolderEdit::Create { id: "b".into(), params: custom("Work") }).await.unwrap();
    let folders = db.chat_folders().await.unwrap();
    assert_eq!(ids(&folders), ["all", "a", "b"]);

    db.edit_chat_folders(ChatFolderEdit::Reorder(vec!["all".into(), "b".into(), "a".into()])).await.unwrap();
    assert_eq!(ids(&db.chat_folders().await.unwrap()), ["all", "b", "a"]);
    // Numbered from 0, as EnsureAll numbers them, and a new one goes last.
    let positions = |folders: &[ChatFolder]| current_folders(folders).into_iter().map(|f| f.position).collect::<Vec<_>>();
    assert_eq!(positions(&db.chat_folders().await.unwrap()), [0, 1, 2]);
    db.edit_chat_folders(ChatFolderEdit::Create { id: "c".into(), params: custom("Family") }).await.unwrap();
    let folders = db.chat_folders().await.unwrap();
    assert_eq!((ids(&folders), positions(&folders)), (vec!["all".to_owned(), "b".into(), "a".into(), "c".into()], vec![0, 1, 2, 3]));
    db.edit_chat_folders(ChatFolderEdit::Delete { id: "c".into(), at: 50 }).await.unwrap();

    // A chat added, then taken out (Desktop's toggle excludes it).
    let chat = conv("d:x");
    db.edit_chat_folders(ChatFolderEdit::ToggleChat { id: "a".into(), chat: chat.clone(), add: true }).await.unwrap();
    db.edit_chat_folders(ChatFolderEdit::ToggleChat { id: "a".into(), chat: chat.clone(), add: false }).await.unwrap();
    let a = db.chat_folders().await.unwrap().into_iter().find(|f| f.id == "a").unwrap();
    assert_eq!((a.included.len(), a.excluded.clone()), (0, vec![chat.clone()]));

    // Deleting keeps it, with when, out of the order.
    db.edit_chat_folders(ChatFolderEdit::Delete { id: "b".into(), at: 77 }).await.unwrap();
    let folders = db.chat_folders().await.unwrap();
    assert_eq!(ids(&folders), ["all", "a"]);
    let b = folders.iter().find(|f| f.id == "b").unwrap();
    assert_eq!((b.deleted_at, b.position), (77, CHAT_FOLDER_DELETED_POSITION));
    assert_eq!(folders.iter().find(|f| f.id == "a").unwrap().position, 1);

    // Storage Service's: the All folder takes the remote's id, a deletion
    // keeps the earlier time, a new folder is added.
    let remote_all = ChatFolder { id: "remote-all".into(), folder_type: ChatFolderType::All, position: 0, ..b.clone() };
    db.edit_chat_folders(ChatFolderEdit::Merge(remote_all)).await.unwrap();
    db.edit_chat_folders(ChatFolderEdit::Merge(ChatFolder { deleted_at: 99, ..b.clone() })).await.unwrap();
    let folders = db.chat_folders().await.unwrap();
    assert!(folders.iter().any(|f| f.id == "remote-all" && f.folder_type == ChatFolderType::All));
    assert!(!folders.iter().any(|f| f.id == "all"));
    assert_eq!(folders.iter().find(|f| f.id == "b").unwrap().deleted_at, 77);
}

#[test]
fn folders_hold_chats_as_desktops_do() {
    let folder = |params: ChatFolderParams| {
        let mut folders = Vec::new();
        apply_folder_edit(&mut folders, ChatFolderEdit::Create { id: "f".into(), params });
        folders.remove(0)
    };
    let (alice, group, me) = (conv("d:alice"), conv("g:hikers"), conv("d:me"));
    let direct = FolderChat { id: &alice, kind: ConversationKind::Direct, unread: false, muted: false };
    let unread_group = FolderChat { id: &group, kind: ConversationKind::Group, unread: true, muted: true };
    let note = FolderChat { id: &me, kind: ConversationKind::NoteToSelf, unread: false, muted: false };

    let unread = folder(ChatFolderParams::unread());
    assert!(!unread.contains(&direct) && unread.contains(&unread_group));
    let ones = folder(ChatFolderParams::individual());
    assert!(ones.contains(&direct) && ones.contains(&note) && !ones.contains(&unread_group));
    // Muted chats left out, unless the menu asks without the filters.
    let groups = folder(ChatFolderParams { show_muted_chats: false, ..ChatFolderParams::groups() });
    assert!(!groups.contains(&unread_group) && groups.contains_with(&unread_group, true));
    let picked = folder(ChatFolderParams { included: vec![alice.clone()], ..ChatFolderParams::defaults() });
    assert!(picked.contains(&direct) && !picked.contains(&note));
    assert!(ChatFolderParams::unread().matches(&unread) && !ChatFolderParams::groups().matches(&unread));
    assert!(!ChatFolderParams::defaults().valid());
    assert!(ChatFolderParams { name: "x".repeat(32), ..Default::default() }.valid());
    assert!(!ChatFolderParams { name: "x".repeat(33), ..Default::default() }.valid());
}

#[tokio::test]
async fn sticker_packs_are_known_installed_and_ordered() {
    let db = test_db().await;
    assert!(db.sticker_pack_seen("aa", "k1", 1).await.unwrap());
    assert!(!db.sticker_pack_seen("aa", "other", 2).await.unwrap(), "a known pack keeps its key");
    db.sticker_pack_seen("bb", "k2", 3).await.unwrap();
    db.set_sticker_pack_meta("aa", "Dots", "Ana", Some("/tmp/c.webp"), 12).await.unwrap();
    let rows = db.sticker_pack_rows().await.unwrap();
    assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["bb", "aa"], "newest first");
    let aa = rows.iter().find(|r| r.id == "aa").unwrap();
    assert_eq!((aa.key.as_str(), aa.title.as_deref(), aa.sticker_count, aa.installed), ("k1", Some("Dots"), 12, false));

    // Installing puts it after every installed pack.
    assert_eq!(db.install_sticker_pack_row("aa", "k1", None, 4).await.unwrap(), (0, true));
    assert_eq!(db.install_sticker_pack_row("cc", "k3", None, 5).await.unwrap(), (1, true));
    assert_eq!(db.install_sticker_pack_row("aa", "k1", Some(7), 6).await.unwrap(), (7, false));
    db.set_sticker_pack_order(&["cc".into(), "aa".into()]).await.unwrap();
    let position = |rows: &[StickerPackRow], id: &str| rows.iter().find(|r| r.id == id).unwrap().position;
    let rows = db.sticker_pack_rows().await.unwrap();
    assert_eq!((position(&rows, "cc"), position(&rows, "aa")), (Some(0), Some(1)));

    assert!(db.uninstall_sticker_pack_row("aa", 9).await.unwrap());
    assert!(!db.uninstall_sticker_pack_row("aa", 10).await.unwrap(), "already uninstalled");
    let aa = db.sticker_pack_rows().await.unwrap().into_iter().find(|r| r.id == "aa").unwrap();
    assert_eq!((aa.installed, aa.position, aa.uninstalled_at), (false, None, Some(9)));
}
