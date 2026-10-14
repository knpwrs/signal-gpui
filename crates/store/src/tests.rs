use super::*;

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
    let db = AppDb::open_in_memory().await.unwrap();
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
}

#[tokio::test]
async fn insert_dedupes_and_tracks_unread_and_preview() {
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
    db.insert_message(&incoming("g:aa", "bob", 10, "hello\nsecond line")).await.unwrap();
    let c = db.conversation(&conv("g:aa")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("bob: hello second line"));
    db.insert_message(&incoming("d:bob", "bob", 10, "hello")).await.unwrap();
    let c = db.conversation(&conv("d:bob")).await.unwrap().unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("hello"));
}

#[tokio::test]
async fn pagination_is_oldest_first() {
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
        remote: None,
    }];
    m.quote = Some(Quote { author: None, author_name: "You".into(), sent_at: 0, text: "q".into(), thumbnail: None });
    let id = db.insert_message(&m).await.unwrap();
    let id2 = db.insert_message(&incoming("d:b", "b", 2, "Crème brûlée foxes")).await.unwrap();

    let got = db.message(id).await.unwrap().unwrap();
    assert_eq!(got.quote.unwrap().text, "q");
    db.set_attachment_path(id, 0, "/tmp/a.png").await.unwrap();
    db.set_attachment_path(id, 5, "/tmp/nope").await.unwrap();
    assert_eq!(
        db.message(id).await.unwrap().unwrap().attachments[0].local_path.as_deref(),
        Some("/tmp/a.png")
    );

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
async fn attachment_only_preview() {
    let db = AppDb::open_in_memory().await.unwrap();
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
        remote: None,
    }];
    db.insert_message(&m).await.unwrap();
    assert_eq!(
        db.conversation(&conv("d:a")).await.unwrap().unwrap().last_message_preview.as_deref(),
        Some("📷 Photo")
    );
}

#[tokio::test]
async fn emoji_usage_and_settings() {
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
    db.replace_contact_names(&[("a".into(), "Jon".into()), ("b".into(), "Bo".into())]).await.unwrap();
    assert_eq!(db.contact_name("a").await.unwrap().as_deref(), Some("Jon"));
    db.replace_contact_names(&[("a".into(), "Jonathan".into())]).await.unwrap();
    assert_eq!(db.contact_name("a").await.unwrap().as_deref(), Some("Jonathan"));
    assert_eq!(db.contact_name("b").await.unwrap(), None);
}

#[tokio::test]
async fn rename_authors_updates_messages_and_quotes() {
    let db = AppDb::open_in_memory().await.unwrap();
    let mut first = incoming("g:01", "jon", 1, "hi");
    first.author_name = None;
    let first = db.insert_message(&first).await.unwrap();
    let mut reply = incoming("g:01", "amy", 2, "re");
    reply.quote = Some(Quote { author: Some("jon".into()), author_name: "jon".into(), sent_at: 1, text: "hi".into(), thumbnail: None });
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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

    // Muted and archived: stays archived when a message arrives.
    db.insert_message(&incoming("d:a", "a", 3, "again")).await.unwrap();
    assert!(db.conversation(&conv("d:a")).await.unwrap().unwrap().archived);
    // Not muted: a new message unarchives.
    db.apply_chat_states(&[state("d:a", true, 0)]).await.unwrap();
    db.insert_message(&incoming("d:a", "a", 4, "once more")).await.unwrap();
    assert!(!db.conversation(&conv("d:a")).await.unwrap().unwrap().archived);
}

#[tokio::test]
async fn directory_search_skips_existing_conversations() {
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
    db.set_expire_timer(&conv("d:a"), 3600, 4).await.unwrap();
    let c = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!((c.expire_timer, c.expire_timer_version), (3600, 4));
}

#[tokio::test]
async fn searching_one_chat_and_paging_around_a_message() {
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
        remote: None,
    }];
    let photo = db.insert_message(&photo).await.unwrap();
    let media: Vec<i64> = db.media_messages(&conv("d:a"), 10).await.unwrap().iter().map(|m| m.id).collect();
    assert_eq!(media, vec![photo]);
}

#[tokio::test]
async fn people_are_direct_chats_and_contacts() {
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    let db = AppDb::open_in_memory().await.unwrap();
    let mut mine = outgoing("d:a", 100, "hi");
    mine.status = DeliveryStatus::Read;
    db.insert_message(&mine).await.unwrap();
    db.insert_notice(&conv("d:a"), 200, &Notice::Timer { by: None, seconds: 60 }).await.unwrap();
    let chat = db.conversation(&conv("d:a")).await.unwrap().unwrap();
    assert_eq!(chat.last_outgoing_status, Some(DeliveryStatus::Read));
}

/// Rewrites the migration history to look like a pre-squash database that
/// ran the first `applied` of the 16 development migrations.
async fn fake_pre_squash_history(db: &AppDb, applied: usize) {
    const VERSIONS: [i64; 16] = [
        20261013000000, 20261014000000, 20261015000000, 20261016000000, 20261017000000, 20261018000000,
        20261019000000, 20261020000000, 20261021000000, 20261022000000, 20261023000000, 20261024000000,
        20261025000000, 20261026000000, 20261027000000, 20261028000000,
    ];
    sqlx::query("DELETE FROM _sqlx_migrations").execute(&db.pool).await.unwrap();
    // Nor had it anything from migrations after the squash.
    sqlx::query("DROP TABLE message_edits").execute(&db.pool).await.unwrap();
    for version in &VERSIONS[..applied] {
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?, 'old', TRUE, x'00', 0)",
        )
        .bind(version)
        .execute(&db.pool)
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
    let db = AppDb::open_in_memory().await.unwrap();
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
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00'").execute(&db.pool).await.unwrap();
    if with_edits_migration {
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (20261029000000, 'message edits', TRUE, x'00', 0)",
        )
        .execute(&db.pool)
        .await
        .unwrap();
    } else {
        sqlx::query("DROP TABLE message_edits").execute(&db.pool).await.unwrap();
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
        let versions: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations").fetch_all(&db.pool).await.unwrap();
        assert_eq!(versions, vec![20261013000000]);
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
