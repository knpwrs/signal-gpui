//! Offline demo backend: seeds sample conversations and echoes sends locally.
//! Used for UI development and screenshots without linking a phone
//! (`SIGNAL_GPUI_DEMO=1`). Never connects to Signal; the only network
//! requests are previews of links you type and, with a Giphy key, GIF
//! search and downloads.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt as _;
use signal_store::{
    AppDb, AttachmentMeta, ConversationId, ConversationKind, LinkPreview, ConversationUpdate, DeliveryStatus, Direction, NewMessage, Quote,
    Reaction,
};

use crate::{Command, ConnectionState, Event};

// Everyone here is made up.
const MAYA: &str = "d:00000000-0000-4000-8000-00000000a11c";
const JORDAN: &str = "d:00000000-0000-4000-8000-0000000000b0";
const PRIYA: &str = "d:00000000-0000-4000-8000-0000000000c0";
const SAM: &str = "d:00000000-0000-4000-8000-0000000000e0";
const ANA: &str = "d:00000000-0000-4000-8000-0000000000f0";
const LEO: &str = "d:00000000-0000-4000-8000-0000000000f1";
const GROUP: &str = "g:0000000000000000000000000000000000000000000000000000000000000001";
const HOUSE: &str = "g:0000000000000000000000000000000000000000000000000000000000000003";
/// A group Maya invited us to (`demo_groups.rs`).
const INVITED: &str = "g:0000000000000000000000000000000000000000000000000000000000000005";
/// How long a vote here shows as sending.
const DEMO_VOTE_SENDING: Duration = Duration::from_millis(800);
const SELF: &str = "d:00000000-0000-4000-8000-000000000001";

const MINUTE: i64 = 60_000;
/// Our demux id in the demo's group calls.
const DEMO_LOCAL_DEMUX: u32 = 100;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// A timestamp for a message we send: now, or just after the last one (a
/// timestamp names a message, so two in one millisecond would collide).
fn next_sent_at() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now();
    let previous = LAST.fetch_max(now, Ordering::SeqCst);
    if previous >= now {
        LAST.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        now
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A service id without the demo's conversation prefix.
fn aci(id: &str) -> &str {
    id.trim_start_matches("d:")
}

async fn seed(db: &AppDb, data_dir: &Path) -> signal_store::Result<()> {
    if !db.conversations().await?.is_empty() {
        return Ok(());
    }
    // The made-up account's phone has these on (as if Storage Service said
    // so; a real account starts with them off until it's read).
    for setting in [crate::AccountSetting::ReadReceipts, crate::AccountSetting::TypingIndicators, crate::AccountSetting::LinkPreviews] {
        db.set_setting(setting.key(), "true").await?;
    }
    let convs = [
        (MAYA, ConversationKind::Direct, "Maya Chen", Some("+1 555 0100")),
        (JORDAN, ConversationKind::Direct, "Jordan Reyes", Some("+1 555 0199")),
        (GROUP, ConversationKind::Group, "Saturday hikes 🥾", None),
        (HOUSE, ConversationKind::Group, "Maple St house 🏠", None),
        (SELF, ConversationKind::NoteToSelf, "Note to Self", None),
        (PRIYA, ConversationKind::Direct, "Priya Natarajan", Some("+1 555 0142")),
        (SAM, ConversationKind::Direct, "Sam Okafor", Some("+1 555 0163")),
        (ANA, ConversationKind::Direct, "Ana Souza", Some("+1 555 0128")),
        (LEO, ConversationKind::Direct, "Leo Martin", Some("+1 555 0187")),
    ];
    for (id, kind, title, phone) in convs {
        db.upsert_conversation(
            &ConversationId(id.into()),
            ConversationUpdate {
                kind: Some(kind),
                title: Some(title.into()),
                phone: phone.map(Into::into),
                ..Default::default()
            },
        )
        .await?;
    }

    // Everyone here is in the made-up phone's address book, and both
    // groups share our profile: none of them is a message request (set
    // before any message, so no chat shows as one while this runs).
    db.replace_system_contacts(&[MAYA, JORDAN, PRIYA, SAM, ANA, LEO].map(|p| aci(p).to_owned())).await?;
    for group in [GROUP, HOUSE] {
        db.set_profile_sharing(&ConversationId(group.into()), true).await?;
    }

    // Group members, for Desktop's per-member name colors.
    db.upsert_conversation(
        &ConversationId(GROUP.into()),
        ConversationUpdate {
            members: Some([MAYA, JORDAN, PRIYA, SAM].map(|m| aci(m).to_owned()).to_vec()),
            admins: Some(vec![aci(MAYA).into()]),
            description: Some("Usually Saturday mornings. Bring water and layers.".into()),
            // Member labels (Signal's), and ours is ours to set.
            member_labels: Some(
                [
                    (aci(MAYA).into(), signal_store::MemberLabel { emoji: Some("🗺️".into()), text: "Route planner".into() }),
                    (aci(JORDAN).into(), signal_store::MemberLabel { emoji: None, text: "Snacks".into() }),
                ]
                .into(),
            ),
            can_label: Some(true),
            // Any member may add people (Desktop's default).
            can_add_members: Some(true),
            ..Default::default()
        },
    )
    .await?;
    db.upsert_conversation(
        &ConversationId(HOUSE.into()),
        ConversationUpdate {
            members: Some(vec![aci(ANA).into(), aci(LEO).into()]),
            // We're its admin, so admin delete can be tried here.
            admins: Some(vec![aci(SELF).into()]),
            ..Default::default()
        },
    )
    .await?;

    crate::demo_groups::seed(db, HOUSE, INVITED, aci(MAYA), aci(JORDAN), aci(PRIYA), aci(SAM)).await?;

    let now = now();
    let name = |id: &str| match id {
        MAYA => "Maya Chen",
        JORDAN => "Jordan Reyes",
        PRIYA => "Priya Natarajan",
        SAM => "Sam Okafor",
        ANA => "Ana Souza",
        _ => "Leo Martin",
    };
    let incoming = |conv: &str, from: &str, at: i64, body: &str| NewMessage {
        conversation_id: ConversationId(conv.into()),
        sent_at: at,
        received_at: at,
        direction: Direction::Incoming,
        author: Some(aci(from).into()),
        author_name: Some(name(from).into()),
        body: body.into(),
        status: DeliveryStatus::Delivered,
        quote: None,
        attachments: vec![],
        link_preview: None,
        expires_in: 0,
    };
    let outgoing = |conv: &str, at: i64, body: &str, status| NewMessage {
        conversation_id: ConversationId(conv.into()),
        sent_at: at,
        received_at: at,
        direction: Direction::Outgoing,
        author: None,
        author_name: None,
        body: body.into(),
        status,
        quote: None,
        attachments: vec![],
        link_preview: None,
        expires_in: 0,
    };
    let quote_of = |message: &NewMessage, thumbnail: Option<String>| Quote {
        author: message.author.clone(),
        author_name: message.author_name.clone().unwrap_or_default(),
        sent_at: message.sent_at,
        text: message.body.clone(),
        thumbnail,
        attachment: message.attachments.first().map(signal_store::QuotedAttachment::of),
        story: false,
    };
    let react = |emoji: &str, from: Option<&str>| Reaction { emoji: emoji.into(), author: from.map(|f| aci(f).into()), sent_at: now };

    // Drawn images (no one's real photos): a sunset, and link preview art.
    let image = |name: &str, width: u32, height: u32| -> Option<AttachmentMeta> {
        let path = data_dir.join("attachments").join(name);
        if let Err(error) = write_demo_photo(&path, width, height) {
            tracing::warn!(%error, "failed to draw a demo image");
            return None;
        }
        Some(AttachmentMeta {
            content_type: "image/png".into(),
            file_name: Some(name.into()),
            size: std::fs::metadata(&path).map_or(0, |m| m.len()),
            width: Some(width),
            height: Some(height),
            local_path: path.to_str().map(Into::into),
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        })
    };
    let photo = image("demo-photo.png", PHOTO_WIDTH, PHOTO_HEIGHT);
    let with_photo = |mut message: NewMessage| {
        message.attachments = photo.clone().into_iter().collect();
        message
    };

    // The hiking group: planning this Saturday's hike.
    let y = now - DAY - 5 * HOUR;
    db.insert_message(&incoming(GROUP, MAYA, y, "Who's up for a hike Saturday? Forecast says sunny and 68°")).await?;
    db.insert_message(&incoming(GROUP, JORDAN, y + 4 * MINUTE, "I'm in")).await?;
    db.insert_message(&incoming(GROUP, PRIYA, y + 9 * MINUTE, "Same, as long as we start early. The exposed stretch gets hot by noon"))
        .await?;
    let added = signal_store::Notice::Group {
        by: Some(aci(MAYA).into()),
        change: signal_store::GroupNotice::Added { member: Some(aci(SAM).into()) },
    };
    db.insert_notice(&ConversationId(GROUP.into()), y + 12 * MINUTE, &added).await?;
    db.insert_message(&incoming(GROUP, SAM, y + 15 * MINUTE, "Thanks for the add! Which trail are we thinking?")).await?;
    let mut trail = incoming(GROUP, MAYA, y + 17 * MINUTE, "The ridge loop, about 7 miles. Conditions: https://www.nps.gov/subjects/trails/");
    trail.link_preview = Some(LinkPreview {
        url: "https://www.nps.gov/subjects/trails/".into(),
        title: Some("Trails".into()),
        description: Some("Conditions, closures and guides for hiking trails.".into()),
        date: None,
        image: image("demo-preview-wide.png", 600, 315),
    });
    db.insert_message(&trail).await?;
    db.insert_message(&incoming(GROUP, JORDAN, y + 21 * MINUTE, "Ridge loop. The lake trail was pure mud last time")).await?;
    // Signal's formatting: one of each style.
    let pack = "Pack list: water, layers, snacks. Gate code is 4417. Best part is the surprise at the top";
    let pack_id = db.insert_message(&incoming(GROUP, PRIYA, y + 30 * MINUTE, pack)).await?;
    let span = |text: &str, style| {
        let start = pack.find(text).unwrap_or_default();
        signal_store::StyleSpan { start, len: text.len(), style }
    };
    db.set_styles(pack_id, &[
        span("Pack list", signal_store::TextStyle::Bold),
        span("layers", signal_store::TextStyle::Italic),
        span("snacks", signal_store::TextStyle::Strikethrough),
        span("4417", signal_store::TextStyle::Monospace),
        span("the surprise at the top", signal_store::TextStyle::Spoiler),
    ])
    .await?;
    let view = with_photo(incoming(GROUP, PRIYA, y + 40 * MINUTE, "View from the top last time. Bring layers, it's windy up there"));
    let view_id = db.insert_message(&view).await?;
    db.set_reaction(view_id, react("😍", Some(MAYA))).await?;
    let mut wow = incoming(GROUP, MAYA, y + 44 * MINUTE, "Okay that settles it");
    wow.quote = Some(quote_of(&view, photo.as_ref().and_then(|p| p.local_path.clone())));
    db.insert_message(&wow).await?;
    db.insert_message(&incoming(GROUP, SAM, now - 2 * HOUR, "I can drive two people from downtown")).await?;
    let ride = db.insert_message(&incoming(GROUP, JORDAN, now - 2 * HOUR + 3 * MINUTE, "@Sam Okafor save me a seat 🙏")).await?;
    db.set_mentions(ride, &[signal_store::MentionSpan { start: 0, len: "@Sam Okafor".len(), aci: aci(SAM).into() }]).await?;
    let plan = db
        .insert_message(&outgoing(GROUP, now - 25 * MINUTE, "Ridge loop it is. Meet at the trailhead lot at 8:30 Saturday?", DeliveryStatus::Read))
        .await?;
    // Four kinds of reaction, so the pills collapse into "+2".
    for (emoji, from) in [("👍", Some(MAYA)), ("👍", Some(JORDAN)), ("❤️", Some(PRIYA)), ("🎉", Some(SAM)), ("🙌", None)] {
        db.set_reaction(plan, react(emoji, from)).await?;
    }
    // Read up to here; one new message after.
    db.mark_read(&ConversationId(GROUP.into())).await?;
    db.insert_message(&incoming(GROUP, PRIYA, now - 22 * MINUTE, "Perfect. I'll bring the first aid kit")).await?;

    // Maya: catching up, and an article she sent.
    let home = incoming(MAYA, MAYA, now - 2 * DAY - 3 * HOUR, "Did you get home okay?");
    let home_id = db.insert_message(&home).await?;
    db.insert_message(&outgoing(MAYA, now - 2 * DAY - 3 * HOUR + 5 * MINUTE, "Yep! Thanks again for dinner", DeliveryStatus::Read)).await?;
    db.insert_message(&incoming(MAYA, MAYA, now - 2 * DAY - 3 * HOUR + 7 * MINUTE, "Anytime. Next one's at our place")).await?;
    // A voice message: synthesized tones, not anyone's voice.
    let voice_path = data_dir.join("attachments").join("demo-voice.wav");
    if std::fs::write(&voice_path, demo_voice_wav()).is_ok() {
        let mut voice = incoming(MAYA, MAYA, now - 2 * DAY - 3 * HOUR + 9 * MINUTE, "");
        voice.attachments = vec![AttachmentMeta {
            content_type: "audio/wav".into(),
            file_name: None,
            size: std::fs::metadata(&voice_path).map_or(0, |m| m.len()),
            width: None,
            height: None,
            local_path: voice_path.to_str().map(Into::into),
            sticker: false,
            gif: false,
            voice: true,
            remote: None,
            thumbnail_path: None,
        }];
        db.insert_message(&voice).await?;
    }
    // A view-once photo, still to be opened (its own drawn file, since
    // opening it deletes it).
    let mut once = incoming(MAYA, MAYA, now - 2 * DAY - 3 * HOUR + 11 * MINUTE, "");
    once.attachments = image("demo-view-once.png", PHOTO_WIDTH, PHOTO_HEIGHT).into_iter().collect();
    db.insert_message_view_once(&once, Some(signal_store::ViewOnce::Ready)).await?;
    db.set_reaction(home_id, react("❤️", None)).await?;
    let mut article = incoming(MAYA, MAYA, now - 50 * MINUTE, "This is the post I was telling you about: https://signal.org/blog/");
    let article_image = image("demo-preview-square.png", 160, 160);
    article.link_preview = Some(LinkPreview {
        url: "https://signal.org/blog/".into(),
        title: Some("Signal Blog".into()),
        description: Some("News and updates.".into()),
        date: Some(now - DAY),
        image: article_image.clone(),
    });
    db.insert_message(&article).await?;
    let mut reply = outgoing(MAYA, now - 45 * MINUTE, "Reading it now", DeliveryStatus::Read);
    reply.quote = Some(quote_of(&article, article_image.and_then(|i| i.local_path)));
    db.insert_message(&reply).await?;
    // She set up a new phone.
    let changed = signal_store::Notice::SafetyNumberChanged { who: aci(MAYA).into() };
    db.insert_notice(&ConversationId(MAYA.into()), now - 30 * MINUTE, &changed).await?;
    db.mark_read(&ConversationId(MAYA.into())).await?;
    db.insert_message(&incoming(MAYA, MAYA, now - 12 * MINUTE, "Let me know what you think!")).await?;

    // Ana: coffee plans, unread.
    db.insert_message(&outgoing(ANA, now - 6 * DAY, "We should get coffee soon, it's been ages", DeliveryStatus::Read)).await?;
    db.insert_message(&incoming(ANA, ANA, now - 6 * DAY + 20 * MINUTE, "Yes! Thursday morning?")).await?;
    db.mark_read(&ConversationId(ANA.into())).await?;
    db.insert_message(&incoming(ANA, ANA, now - 3 * HOUR, "Are we still on for Thursday? The place on 5th opens at 7")).await?;

    // The house group: muted, a few unread.
    db.insert_message(&incoming(HOUSE, LEO, now - 5 * HOUR, "Rent's due Friday, I sent my half")).await?;
    db.insert_message(&incoming(HOUSE, ANA, now - 4 * HOUR, "Same. Also we're out of dish soap again")).await?;
    db.insert_message(&incoming(HOUSE, LEO, now - 4 * HOUR + 2 * MINUTE, "I'll grab some on the way home")).await?;
    // A poll: Ana asks, Leo has voted for two evenings, Ana for one.
    let chores = "Which night works for a deep clean?";
    let poll_id = db.insert_message(&incoming(HOUSE, ANA, now - 3 * HOUR, chores)).await?;
    let mut poll = signal_store::Poll::new(chores.into(), vec!["Tuesday".into(), "Wednesday".into(), "Sunday".into()], true);
    for (from, options) in [(LEO, vec![0, 2]), (ANA, vec![2])] {
        let vote = signal_store::PollVote { author: Some(aci(from).into()), options, vote_count: 1, sent_at: now - 3 * HOUR, ..Default::default() };
        poll.apply_vote(vote, signal_store::VoteSource::Other);
    }
    db.set_poll(poll_id, &poll).await?;
    db.set_muted(&ConversationId(HOUSE.into()), true).await?;

    // The Signal chat: a release note (made up), with Desktop's donate button.
    let signal = ConversationId::signal();
    db.upsert_conversation(&signal, ConversationUpdate { kind: Some(ConversationKind::Direct), title: Some("Signal".into()), ..Default::default() })
        .await?;
    db.set_muted(&signal, true).await?;
    let title = "Welcome to the demo";
    let body = format!("{title}\n\nRelease notes from Signal show up in this chat. It's read-only, and muted to start with.");
    let mut note = incoming(MAYA, MAYA, now - 9 * DAY, &body);
    note.conversation_id = signal.clone();
    note.author = Some(signal_store::SIGNAL_ACI.into());
    note.author_name = Some("Signal".into());
    let note = db.insert_message(&note).await?;
    db.set_styles(note, &[signal_store::StyleSpan { start: 0, len: title.len(), style: signal_store::TextStyle::Bold }]).await?;
    db.set_cta(note, &signal_store::CallToAction { id: "donate".into(), text: Some("Donate Now".into()) }).await?;
    db.mark_read(&signal).await?;

    // Jordan: borrowing gear, with a draft waiting.
    db.insert_message(&incoming(JORDAN, JORDAN, now - 4 * DAY, "Can I borrow your tent for the 20th?")).await?;
    db.insert_message(&outgoing(JORDAN, now - 4 * DAY + 10 * MINUTE, "Of course. It's in the garage, I'll leave it out", DeliveryStatus::Read))
        .await?;
    // A GIF (a short looping video), drawn by ffmpeg when it's installed.
    let clip = data_dir.join("attachments").join("demo-gif.mp4");
    let made = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "ffmpeg"].iter().any(|ffmpeg| {
        std::process::Command::new(ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "gradients=size=320x180:rate=15:duration=2:speed=0.05", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success())
    });
    if made {
        let mut gif = incoming(JORDAN, JORDAN, now - 4 * DAY + 12 * MINUTE, "");
        gif.attachments = vec![AttachmentMeta {
            content_type: "video/mp4".into(),
            file_name: None,
            size: std::fs::metadata(&clip).map_or(0, |m| m.len()),
            width: Some(320),
            height: Some(180),
            local_path: clip.to_str().map(Into::into),
            sticker: false,
            gif: true,
            voice: false,
            remote: None,
            thumbnail_path: None,
        }];
        db.insert_message(&gif).await?;
    }
    db.insert_message(&incoming(JORDAN, JORDAN, now - 4 * DAY + 13 * MINUTE, "You're the best 🙏")).await?;
    db.mark_read(&ConversationId(JORDAN.into())).await?;
    db.set_draft(&ConversationId(JORDAN.into()), Some("No rush on the tent, I won't need it until")).await?;

    // Note to Self: a list, and photos alone, with a caption, and with a
    // long caption.
    db.insert_message(&outgoing(SELF, now - 10 * DAY, "Groceries: oat milk, coffee, lemons, rice ☕️", DeliveryStatus::Sent)).await?;
    if photo.is_some() {
        db.insert_message(&with_photo(outgoing(SELF, now - 3 * DAY, "", DeliveryStatus::Sent))).await?;
        db.insert_message(&with_photo(outgoing(SELF, now - 3 * DAY + MINUTE, "Sunset from the trailhead", DeliveryStatus::Sent))).await?;
        db.insert_message(&with_photo(outgoing(
            SELF,
            now - 3 * DAY + 2 * MINUTE,
            "Notes from the hike:\n\nThe first two miles climb steadily through pine forest, then the trail opens onto a ridge with views in every direction. Bring layers: it was ten degrees cooler at the top.\nThe last mile down is steep and loose, so poles help.",
            DeliveryStatus::Sent,
        )))
        .await?;
    }

    // Priya, Sam and Leo: older 1:1s; Priya's is archived.
    db.insert_message(&incoming(PRIYA, PRIYA, now - 20 * DAY, "Thanks for the help with the move!")).await?;
    db.insert_message(&outgoing(PRIYA, now - 20 * DAY + 30 * MINUTE, "Of course! The new place looks great", DeliveryStatus::Read)).await?;
    db.set_archived(&ConversationId(PRIYA.into()), true).await?;
    db.insert_message(&outgoing(SAM, now - 9 * DAY, "Great meeting you at Maya's!", DeliveryStatus::Read)).await?;
    db.insert_message(&incoming(SAM, SAM, now - 9 * DAY + HOUR, "You too! Let me know next time you all go hiking")).await?;
    db.insert_message(&incoming(LEO, LEO, now - 12 * DAY, "Did you take the recycling out?")).await?;
    db.insert_message(&outgoing(LEO, now - 12 * DAY + 4 * MINUTE, "Yep, this morning", DeliveryStatus::Read)).await?;
    for read in [PRIYA, SAM, LEO, SELF] {
        db.mark_read(&ConversationId(read.into())).await?;
    }

    // Names as Storage Service gives them. Leo's profile says "Leonardo
    // Martin"; we gave him a nickname (what his chat is titled) and a note.
    let people = [MAYA, JORDAN, PRIYA, SAM, ANA, LEO];
    let names: Vec<(String, String)> = people.iter().map(|p| (aci(p).to_owned(), name(p).to_owned())).collect();
    let leo = signal_store::ContactNickname {
        given: "Leo".into(),
        family: "Martin".into(),
        note: "Rent is split 50/50, due on the 1st. Allergic to cats.".into(),
        plain_name: Some("Leonardo Martin".into()),
    };
    db.replace_contacts(&names, &[(aci(LEO).to_owned(), leo)]).await?;

    db.replace_pinned(&[ConversationId(GROUP.into()), ConversationId(SELF.into())]).await?;
    // People and groups without a chat yet: found by search.
    db.replace_directory(&[
        signal_store::DirectoryEntry {
            id: ConversationId("d:00000000-0000-4000-8000-0000000000d5".into()),
            kind: ConversationKind::Direct,
            title: "Dana Brooks".into(),
            phone: Some("+15550177".into()),
        },
        signal_store::DirectoryEntry {
            id: ConversationId("g:0000000000000000000000000000000000000000000000000000000000000002".into()),
            kind: ConversationKind::Group,
            title: "Book club 📚".into(),
            phone: None,
        },
    ])
    .await?;
    seed_message_requests(db, now).await?;
    seed_calls(db, now).await?;
    Ok(())
}

/// A made-up stranger, using Maya's name: their message request.
pub(crate) const STRANGER: &str = "d:00000000-0000-4000-8000-0000000000a5";
/// A message request from someone we don't know, with a contact's name,
/// so the name collision warning shows too. (The group we're invited to,
/// the other kind, is `demo_groups`'.)
async fn seed_message_requests(db: &AppDb, now: i64) -> signal_store::Result<()> {
    let stranger = ConversationId(STRANGER.into());
    db.upsert_conversation(
        &stranger,
        ConversationUpdate { kind: Some(ConversationKind::Direct), title: Some("Maya Chen".into()), ..Default::default() },
    )
    .await?;
    for (at, body) in [
        (now - 40 * MINUTE, "Hi! It's Maya, I got a new phone 📱"),
        (now - 39 * MINUTE, "Can you send me the 6-digit code you just got? I need it to get back into my account"),
    ] {
        db.insert_message(&NewMessage {
            conversation_id: stranger.clone(),
            sent_at: at,
            received_at: at,
            direction: Direction::Incoming,
            author: Some(aci(STRANGER).into()),
            author_name: Some("Maya Chen".into()),
            body: body.into(),
            status: DeliveryStatus::Delivered,
            quote: None,
            attachments: vec![],
            link_preview: None,
            expires_in: 0,
        })
        .await?;
    }

    Ok(())
}

/// The demo's call link ("Book club"), ours: a made-up 16-byte root key.
const DEMO_LINK_KEY: [u8; 16] = [0x3c, 0x91, 0x5e, 0x07, 0xa2, 0x6d, 0xf4, 0x18, 0x8b, 0x2e, 0x73, 0xc5, 0x19, 0xd0, 0x64, 0xb7];

/// A link's room id in hex, as the store keeps it.
fn demo_room(key: &[u8]) -> String {
    signal_calls::room_id(key).map(hex::encode).unwrap_or_default()
}

/// Calls for the Calls tab: a missed call from Sam, a video call with
/// Maya, and a call link of ours we called through yesterday. (Older than
/// each chat's last message, so previews stay as they were.)
async fn seed_calls(db: &AppDb, now: i64) -> signal_store::Result<()> {
    use signal_store::{CallOutcome, Notice};
    let missed = Notice::Call { incoming: true, video: false, outcome: CallOutcome::Missed, call_id: None, unseen: false };
    db.insert_notice(&ConversationId(SAM.into()), now - 10 * DAY, &missed).await?;
    let video = Notice::Call { incoming: false, video: true, outcome: CallOutcome::Accepted, call_id: None, unseen: false };
    db.insert_notice(&ConversationId(MAYA.into()), now - 3 * DAY, &video).await?;
    let room_id = demo_room(&DEMO_LINK_KEY);
    db.add_call_link(&signal_store::CallLink {
        room_id: room_id.clone(),
        root_key: DEMO_LINK_KEY.to_vec(),
        admin_key: Some(vec![0x42; 16]),
        name: "Book club".into(),
        restrictions: signal_store::CallLinkRestrictions::AdminApproval,
        revoked: false,
        expiration: Some(now + 90 * DAY),
        deleted_at: None,
        added_at: now - 5 * DAY,
    })
    .await?;
    let call = signal_store::AdhocCall { room_id, call_id: 1, at: now - DAY, status: signal_store::AdhocCallStatus::Joined };
    db.record_adhoc_call(&call).await?;
    Ok(())
}

/// A new link's root key: random, but valid (no "repeated" byte pairs).
fn new_demo_link_key() -> Vec<u8> {
    use rand::Rng as _;
    loop {
        let key: [u8; 16] = rand::rng().random();
        if signal_calls::room_id(&key).is_some() {
            return key.to_vec();
        }
    }
}

const STICKER_SIZE: u32 = 256;

/// A pack of abstract round "stickers" (drawn here, so no artwork ships).
fn demo_stickers(data_dir: &Path) -> Vec<crate::StickerPackView> {
    let dir = data_dir.join("stickers").join("demo");
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    let emoji = ["😀", "😎", "🎉", "❤️", "👍", "🔥", "🌄", "☕"];
    let stickers = emoji
        .iter()
        .enumerate()
        .filter_map(|(ix, emoji)| {
            let path = dir.join(format!("{ix}.png"));
            if !path.exists() {
                write_demo_sticker(&path, ix as f32 / emoji.len() as f32).ok()?;
            }
            Some(crate::StickerView { id: ix as u32, emoji: Some((*emoji).into()), path })
        })
        .collect();
    vec![crate::StickerPackView {
        id: vec![0xde, 0x70],
        key: vec![0; 32],
        title: "Demo shapes".into(),
        author: "signal-gpui".into(),
        stickers,
    }]
}

/// A soft disc of colour on a transparent background.
fn write_demo_sticker(path: &Path, hue: f32) -> std::io::Result<()> {
    let n = STICKER_SIZE as usize;
    let center = n as f32 / 2.;
    let (r, g, b) = hsl_to_rgb(hue, 0.65, 0.55);
    let mut pixels = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt() / center;
            let shade = 1.15 - 0.4 * d;
            let alpha = ((0.95 - d) * 40.).clamp(0., 1.);
            pixels.extend([r * shade, g * shade, b * shade].map(|c| (c * 255.).clamp(0., 255.) as u8));
            pixels.push((alpha * 255.) as u8);
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, STICKER_SIZE, STICKER_SIZE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut writer| writer.write_image_data(&pixels)).map_err(std::io::Error::other)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1. - (2. * l - 1.).abs()) * s;
    let x = c * (1. - ((h * 6.) % 2. - 1.).abs());
    let (r, g, b) = match (h * 6.) as u32 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    let m = l - c / 2.;
    (r + m, g + m, b + m)
}

const PHOTO_WIDTH: u32 = 600;
const PHOTO_HEIGHT: u32 = 800;

/// An abstract "sunset" (gradient sky, sun, hills) so the demo shows photo
/// bubbles without shipping anyone's pictures.
/// Made-up stories, posted in the last day (they last one, so they're
/// posted again whenever none are left): a text story on a gradient, a
/// drawn photo, a group story with replies, and one of ours.
async fn seed_stories(db: &AppDb, data_dir: &Path) -> signal_store::Result<()> {
    use signal_store::{NewStory, StoryBackground, StoryContent, StoryFont, StoryReply, TextStory};
    let now = now();
    if !db.stories(now).await?.is_empty() {
        return Ok(());
    }
    let story = |author: Option<&str>, name: Option<&str>, ago: i64, group: Option<&str>, content: StoryContent| NewStory {
        author: author.map(|a| aci(a).into()),
        author_name: name.map(Into::into),
        sent_at: now - ago,
        received_at: now - ago,
        group: group.map(|g| ConversationId(g.into())),
        content,
        pointer: None,
        allows_replies: true,
        read: author.is_none(),
        recipients: Vec::new(),
    };
    let text = |text: &str, font, background| {
        StoryContent::Text(TextStory { text: text.into(), font, foreground: None, text_background: None, background, preview: None })
    };
    // Desktop's BG6 gradient.
    let sunrise = StoryBackground::Gradient {
        colors: vec![0xff19a9fa, 0xff7097d7, 0xffd1998d, 0xffffc369],
        positions: vec![0., 0.33, 0.66, 1.],
        angle: 180,
    };
    db.insert_story(&story(Some(MAYA), Some("Maya Chen"), 2 * HOUR, None, text("Trail's clear all the way to the lake 🏞️", StoryFont::Bold, sunrise)), now)
        .await?;
    let path = data_dir.join("attachments").join("demo-story.png");
    if write_demo_photo(&path, 540, 960).is_ok() {
        let photo = AttachmentMeta {
            content_type: "image/png".into(),
            file_name: None,
            size: std::fs::metadata(&path).map_or(0, |m| m.len()),
            width: Some(540),
            height: Some(960),
            local_path: path.to_str().map(Into::into),
            sticker: false,
            gif: false,
            voice: false,
            remote: None,
            thumbnail_path: None,
        };
        let content = StoryContent::Media { attachment: photo, caption: "Golden hour".into(), styles: Vec::new() };
        db.insert_story(&story(Some(JORDAN), Some("Jordan Reyes"), 5 * HOUR, None, content), now).await?;
    }
    let group = story(Some(PRIYA), Some("Priya Natarajan"), 3 * HOUR, Some(GROUP), text("Who's in for Saturday?", StoryFont::Serif, StoryBackground::Color(0xff2c5f45)));
    db.insert_story(&group, now).await?;
    let reply = StoryReply { author: Some(aci(SAM).into()), author_name: Some("Sam Okafor".into()), sent_at: now - 2 * HOUR, body: "Me!".into(), emoji: None, deleted: false };
    db.add_story_reply(Some(aci(PRIYA)), group.sent_at, &reply).await?;
    // Ours, to My Story: seen by two, with a reply and a reaction.
    let mine = NewStory {
        recipients: [MAYA, JORDAN, SAM]
            .iter()
            .map(|p| signal_store::StoryRecipient {
                service_id: aci(p).into(),
                list_ids: vec![crate::MY_STORY_ID.into()],
                allows_replies: true,
            })
            .collect(),
        ..story(None, None, HOUR, None, text("Packing for the weekend", StoryFont::Script, StoryBackground::Color(0xff4437d8)))
    };
    db.insert_story(&mine, now).await?;
    db.add_story_view(mine.sent_at, aci(MAYA), now - 50 * MINUTE).await?;
    db.add_story_view(mine.sent_at, aci(JORDAN), now - 20 * MINUTE).await?;
    let reply = |who: &str, name: &str, ago: i64, body: &str, emoji: bool| StoryReply {
        author: Some(aci(who).into()),
        author_name: Some(name.into()),
        sent_at: now - ago,
        body: body.into(),
        emoji: emoji.then(|| body.into()),
        deleted: false,
    };
    db.add_story_reply(None, mine.sent_at, &reply(MAYA, "Maya Chen", 45 * MINUTE, "Where to?", false)).await?;
    db.add_story_reply(None, mine.sent_at, &reply(JORDAN, "Jordan Reyes", 15 * MINUTE, "😮", true)).await?;
    Ok(())
}

/// Who a demo story list sends to, as the real backend works it out
/// (`list_members`): My Story as a block list is everyone in the demo's
/// contacts but those it names.
async fn demo_list_members(db: &AppDb, list: &crate::StoryList) -> Vec<String> {
    if list.is_my_story() && list.block_list {
        let people = db.people().await.unwrap_or_default();
        people.into_iter().filter_map(|p| p.id.as_direct().map(str::to_owned)).filter(|p| !list.members.contains(p)).collect()
    } else {
        list.members.clone()
    }
}

async fn demo_story_lists(db: &AppDb) -> Vec<crate::StoryList> {
    db.setting(crate::SETTING_STORY_LISTS).await.ok().flatten().and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default()
}

fn write_demo_photo(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    let (w, h) = (width as usize, height as usize);
    // Drawn at 600×800 and scaled to the requested size.
    let (sx, sy) = (600. / w as f32, 800. / h as f32);
    let mut pixels = Vec::with_capacity(w * h * 3);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    for y in 0..h {
        let t = y as f32 / h as f32;
        for x in 0..w {
            let (fx, fy) = (x as f32 * sx, y as f32 * sy);
            // Sky: deep blue at the top to orange at the horizon.
            let mut rgb = [lerp(40., 250., t), lerp(60., 150., t), lerp(140., 90., t)];
            let sun = ((fx - 380.).powi(2) + (fy - 430.).powi(2)).sqrt();
            if sun < 70. {
                rgb = [255., 225., 150.];
            } else if sun < 140. {
                let glow = 1. - (sun - 70.) / 70.;
                for (c, target) in rgb.iter_mut().zip([255., 200., 140.]) {
                    *c = lerp(*c, target, glow * 0.6);
                }
            }
            let far_hill = 520. + 40. * (fx / 90.).sin();
            let near_hill = 610. + 50. * (fx / 140. + 1.).cos();
            if fy > near_hill {
                rgb = [lerp(40., 20., t), lerp(70., 40., t), lerp(55., 35., t)];
            } else if fy > far_hill {
                rgb = [90., 80., 110.];
            }
            pixels.extend(rgb.map(|c| c.clamp(0., 255.) as u8));
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut writer| writer.write_image_data(&pixels)).map_err(std::io::Error::other)
}

/// The demo's remote config (see `crate::remote_config`).
const DEMO_REMOTE_CONFIG: &[(&str, &str)] = &[
    ("desktop.adminDelete.receive.prod", "8.30.0"),
    ("desktop.adminDelete.send.prod", "8.30.0"),
    ("desktop.stickerReply.send.prod", "8.30.0"),
    ("desktop.groupMemberLabels.edit.prod", "8.30.0"),
    ("desktop.groupTerminate.send.prod", "8.30.0"),
    ("desktop.keyTransparency.prod", "8.30.0"),
];

pub(crate) async fn run(db: AppDb, data_dir: PathBuf, mut commands: mpsc::UnboundedReceiver<Command>, events: mpsc::UnboundedSender<Event>) {
    let emit = |e: Event| {
        let _ = events.unbounded_send(e);
    };
    // A remote config with the features this client follows turned on
    // (made-up versions), except 1:1 polls, so the demo shows what an
    // account sees once the server sets them.
    crate::remote_config::set_current(crate::RemoteConfig::from_pairs(DEMO_REMOTE_CONFIG.iter().copied()));
    if let Err(e) = seed(&db, &data_dir).await {
        emit(Event::Error(format!("seeding demo data: {e}")));
    }
    if let Err(e) = seed_stories(&db, &data_dir).await {
        emit(Event::Error(format!("seeding demo stories: {e}")));
    }
    emit(Event::Ready {
        phone: Some("+1 555 0123 (demo)".into()),
        aci: SELF.trim_start_matches("d:").into(),
    });
    emit(Event::Connection(ConnectionState::Online));
    // A made-up megaphone, with a drawn image.
    let megaphone_image = data_dir.join("attachments").join("demo-megaphone.png");
    let mut megaphone = (write_demo_sticker(&megaphone_image, 0.62).is_ok()).then(|| crate::megaphones::MegaphoneView {
        id: "demo-megaphone".into(),
        title: "A banner from Signal".into(),
        body: "Signal's megaphones show here, under your chats.".into(),
        image_path: megaphone_image.to_string_lossy().into_owned(),
        primary: Some((crate::megaphones::MegaphoneCta::Finish, "Got it".into())),
        secondary: Some((crate::megaphones::MegaphoneCta::Snooze, "Not now".into())),
    });
    emit(Event::Megaphones(megaphone.clone().into_iter().collect()));
    // A pretend call: (the call, when it started, whether it connected).
    let call: std::rc::Rc<std::cell::RefCell<Option<(crate::CallUpdate, i64, bool)>>> = Default::default();

    while let Some(command) = commands.next().await {
        if crate::demo_groups::handle(&db, &data_dir, &command, &emit).await {
            continue;
        }
        match command {
            // Calls ring, then connect, without going anywhere.
            Command::StartCall { conversation, video } => {
                if call.borrow().is_some() {
                    continue;
                }
                // A group's call has its members in it, cameras off.
                let members = match conversation.as_group() {
                    Some(_) => db.conversation(&conversation).await.ok().flatten().map(|c| c.members).unwrap_or_default(),
                    None => Vec::new(),
                };
                // A call link's call: Leo asks to join one of ours.
                let link = match conversation.as_call_link() {
                    Some(room) => match db.call_link(room).await.ok().flatten() {
                        Some(link) => Some(crate::CallLinkCall {
                            admin: link.is_admin(),
                            pending: if link.is_admin() { vec![aci(LEO).to_owned()] } else { Vec::new() },
                            waiting: false,
                        }),
                        None => continue,
                    },
                    None => None,
                };
                let group = conversation.as_group().is_some() || link.is_some();
                let update = crate::CallUpdate {
                    conversation,
                    status: if group { crate::CallStatus::Connecting } else { crate::CallStatus::Calling },
                    video,
                    incoming: false,
                    remote_video: false,
                    remote_audio: true,
                    sharing: false,
                    remote_sharing: false,
                    group,
                    participants: members
                        .into_iter()
                        .enumerate()
                        .map(|(ix, aci)| crate::CallParticipant {
                            aci,
                            demux_id: ix as u32 + 1,
                            video: false,
                            audio: ix % 2 == 0,
                            presenting: false,
                        })
                        .collect(),
                    ringer: None,
                    raised_hands: Vec::new(),
                    local_demux_id: group.then_some(DEMO_LOCAL_DEMUX),
                    link,
                };
                let started = next_sent_at();
                *call.borrow_mut() = Some((update.clone(), started, false));
                emit(Event::Call(Some(update)));
                let (call, events) = (call.clone(), events.clone());
                tokio::task::spawn_local(async move {
                    // A group's call has no one to ring: it connects.
                    let steps: &[(u64, crate::CallStatus)] = if group {
                        &[(1, crate::CallStatus::Connected { since: 0 })]
                    } else {
                        &[(1, crate::CallStatus::RingingThem), (2, crate::CallStatus::Connected { since: 0 })]
                    };
                    for (delay, status) in steps.iter().cloned() {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                        let mut current = call.borrow_mut();
                        // Only this call: one hung up quickly leaves its timer behind.
                        let Some((update, _, connected)) = current.as_mut().filter(|(_, at, _)| *at == started) else { return };
                        update.status = match status {
                            crate::CallStatus::Connected { .. } => {
                                *connected = true;
                                update.remote_video = update.video;
                                crate::CallStatus::Connected { since: now() }
                            }
                            status => status,
                        };
                        let _ = events.unbounded_send(Event::Call(Some(update.clone())));
                    }
                });
            }
            Command::HangUp | Command::DeclineCall => {
                let Some((update, started, connected)) = call.borrow_mut().take() else { continue };
                // A call link's call goes in the call history.
                if let Some(room_id) = update.conversation.as_call_link() {
                    let call = signal_store::AdhocCall {
                        room_id: room_id.to_owned(),
                        call_id: started,
                        at: started,
                        status: signal_store::AdhocCallStatus::Joined,
                    };
                    if db.record_adhoc_call(&call).await.is_ok() {
                        emit(Event::CallLinksChanged);
                    }
                    emit(Event::Call(None));
                    continue;
                }
                // A group call's notice is "You started a group call", from the start.
                if update.group {
                    // In the call history as a real one is: ringing while
                    // we're in it (ours), then left.
                    let era = format!("{started:016x}");
                    let record = signal_store::GroupCallRecord {
                        call_id: signal_calls::call_id_from_era(&era),
                        era: Some(era),
                        by: None,
                        incoming: false,
                        at: started,
                    };
                    let mut id = None;
                    for (event, active) in [(signal_store::CallHistoryEvent::Ringing, true), (signal_store::CallHistoryEvent::Started, false)] {
                        if let Ok(Some(change)) = db.apply_group_call_event(&update.conversation, &record, event, active, false).await {
                            id = Some(change.id);
                        }
                    }
                    if let Some(id) = id {
                        emit(Event::MessageChanged { conversation: update.conversation.clone(), message_id: id });
                    }
                    emit(Event::Call(None));
                    continue;
                }
                let outcome = if connected { signal_store::CallOutcome::Accepted } else { signal_store::CallOutcome::Unanswered };
                let notice = signal_store::Notice::Call { incoming: false, video: update.video, outcome, call_id: None, unseen: false };
                if let Ok(Some(id)) = db.record_call(&update.conversation, started, &notice).await {
                    emit(Event::MessageChanged { conversation: update.conversation.clone(), message_id: id });
                }
                emit(Event::Call(None));
            }
            Command::AcceptCall { .. } | Command::SetCallAudio(_) | Command::SetCallVideo(_) | Command::ShowGroupVideo { .. } => {}
            // A group call's extras, as RingRTC answers them: our reaction
            // comes back to us, our hand goes up, and a mute reaches them.
            Command::CallReact { emoji } => {
                if call.borrow().as_ref().is_some_and(|(update, _, _)| update.group) {
                    emit(Event::CallReactions(vec![crate::CallReaction { aci: aci(SELF).into(), demux_id: DEMO_LOCAL_DEMUX, emoji }]));
                }
            }
            Command::RaiseHand(raise) => {
                let mut current = call.borrow_mut();
                if let Some((update, _, _)) = current.as_mut().filter(|(update, _, _)| update.group) {
                    update.raised_hands.retain(|d| *d != DEMO_LOCAL_DEMUX);
                    if raise {
                        update.raised_hands.push(DEMO_LOCAL_DEMUX);
                    }
                    emit(Event::Call(Some(update.clone())));
                }
            }
            Command::RemoteMute { demux_id } => {
                let mut current = call.borrow_mut();
                if let Some((update, _, _)) = current.as_mut()
                    && let Some(p) = update.participants.iter_mut().find(|p| p.demux_id == demux_id && p.audio)
                {
                    p.audio = false;
                    let target = p.aci.clone();
                    emit(Event::Call(Some(update.clone())));
                    emit(Event::CallMuted { by: aci(SELF).into(), target: Some(target) });
                }
            }
            // Maya's in the hiking group's call; no one's in the others'.
            Command::PeekGroupCall { conversation } => {
                let users = if conversation.0 == GROUP { vec![aci(MAYA).to_owned()] } else { Vec::new() };
                emit(Event::GroupPeek { conversation, users });
            }
            Command::CreateCallLink => {
                let key = new_demo_link_key();
                let room_id = demo_room(&key);
                let at = now();
                let link = signal_store::CallLink {
                    room_id: room_id.clone(),
                    root_key: key,
                    admin_key: Some(vec![0x42; 16]),
                    restrictions: signal_store::CallLinkRestrictions::AdminApproval,
                    expiration: Some(at + 90 * DAY),
                    added_at: at,
                    ..Default::default()
                };
                if db.add_call_link(&link).await.is_ok() {
                    let placeholder =
                        signal_store::AdhocCall { room_id: room_id.clone(), call_id: at, at, status: signal_store::AdhocCallStatus::Pending };
                    let _ = db.record_adhoc_call(&placeholder).await;
                    emit(Event::CallLinksChanged);
                    emit(Event::CallLinkCreated(room_id));
                }
            }
            Command::UpdateCallLink { room_id, name, restrictions } => {
                let Ok(Some(link)) = db.call_link(&room_id).await else { continue };
                let name = name.map(|n| crate::actor::clamp_link_name(&n)).unwrap_or(link.name);
                let restrictions = restrictions.unwrap_or(link.restrictions);
                if db.set_call_link_state(&room_id, &name, restrictions, false, link.expiration).await.is_ok() {
                    emit(Event::CallLinksChanged);
                }
            }
            Command::DeleteCallLink { room_id } => {
                let Ok(Some(link)) = db.call_link(&room_id).await else { continue };
                let done = if link.is_admin() {
                    db.mark_call_link_deleted(&room_id, now()).await.map(|_| ())
                } else {
                    db.remove_call_link(&room_id).await
                };
                if done.is_ok() {
                    emit(Event::CallLinksChanged);
                }
            }
            Command::OpenCallLink { url } => {
                let Some(key) = signal_calls::parse_link(&url) else {
                    emit(Event::Error("can't open the call link: it isn't a valid call link".into()));
                    continue;
                };
                let room_id = demo_room(&key);
                let link = signal_store::CallLink {
                    room_id: room_id.clone(),
                    root_key: key,
                    restrictions: signal_store::CallLinkRestrictions::None,
                    expiration: Some(now() + 90 * DAY),
                    added_at: now(),
                    ..Default::default()
                };
                if db.add_call_link(&link).await.is_ok() {
                    emit(Event::CallLinksChanged);
                    emit(Event::CallLinkOpened(room_id));
                }
            }
            // Leo, let in, joins with his camera off.
            Command::ApproveCallJoin { aci: who, approve } => {
                let mut current = call.borrow_mut();
                let Some((update, _, _)) = current.as_mut() else { continue };
                let Some(link) = update.link.as_mut() else { continue };
                link.pending.retain(|p| *p != who);
                if approve {
                    let demux_id = update.participants.len() as u32 + 1;
                    update.participants.push(crate::CallParticipant { aci: who, demux_id, video: false, audio: true, presenting: false });
                }
                emit(Event::Call(Some(update.clone())));
            }
            Command::RemoveFromCall { demux_id } => {
                let mut current = call.borrow_mut();
                let Some((update, _, _)) = current.as_mut() else { continue };
                update.participants.retain(|p| p.demux_id != demux_id);
                emit(Event::Call(Some(update.clone())));
            }
            Command::LoadShareSources => {
                let source = |id: &str, name: &str, is_display| crate::ShareSource { id: id.into(), name: name.into(), is_display };
                emit(Event::ShareSources(Ok(vec![
                    source("display:1", "Entire screen", true),
                    source("window:1", "Notes: Trail plan", false),
                ])));
            }
            Command::StartSharing { .. } | Command::StopSharing => {
                let mut current = call.borrow_mut();
                if let Some((update, _, _)) = current.as_mut() {
                    update.sharing = matches!(command, Command::StartSharing { .. });
                    emit(Event::Call(Some(update.clone())));
                }
            }
            // Made-up devices; choices are saved like the real ones.
            Command::LoadCallDevices | Command::SetCallDevice { .. } => {
                if let Command::SetCallDevice { kind, id } = &command {
                    let _ = db.set_setting(kind.setting(), id).await;
                }
                let device = |id: &str, name: &str| crate::Device { id: id.into(), name: name.into() };
                let saved = async |kind: crate::DeviceKind| db.setting(kind.setting()).await.ok().flatten().filter(|s| !s.is_empty());
                emit(Event::CallDevices(crate::CallDevices {
                    devices: crate::Devices {
                        microphones: vec![device("mic-1", "MacBook Pro Microphone"), device("mic-2", "USB Microphone")],
                        speakers: vec![device("out-1", "MacBook Pro Speakers"), device("out-2", "Headphones")],
                        cameras: vec![device("FaceTime HD Camera", "FaceTime HD Camera")],
                    },
                    microphone: saved(crate::DeviceKind::Microphone).await,
                    speaker: saved(crate::DeviceKind::Speaker).await,
                    camera: saved(crate::DeviceKind::Camera).await,
                }));
            }
            // The one thing the demo fetches: previews of links you type.
            Command::FetchLinkPreview { url } => {
                let events = events.clone();
                let on = crate::setting_on(db.setting(crate::SETTING_LINK_PREVIEWS).await.ok().flatten().as_deref());
                tokio::task::spawn_local(async move {
                    let preview = match on {
                        true => crate::link_preview::fetch(&url, crate::media::Optimization::Max).await.unwrap_or(None),
                        false => None,
                    };
                    let _ = events.unbounded_send(Event::LinkPreview { url, preview });
                });
            }
            Command::SendText { conversation, body, quote, preview, mentions, styles } => {
                let (body, spans, styles) = demo_mentions(&db, &body, &mentions, &styles).await;
                let quote = match quote {
                    Some(id) => db.message(id).await.ok().flatten().map(|m| Quote {
                        author: m.author.clone(),
                        author_name: m.author_name.clone().unwrap_or_default(),
                        sent_at: m.sent_at,
                        text: m.body.clone(),
                        thumbnail: m.thumbnail(),
                        attachment: m.attachments.first().map(signal_store::QuotedAttachment::of),
                        story: false,
                    }),
                    None => None,
                };
                let sent_at = next_sent_at();
                let link_preview = preview.filter(|p| crate::link_preview::is_valid(&body, &p.url)).map(|draft| {
                    let image = draft.image.and_then(|image| {
                        let ext = crate::media::ImageKind::sniff(&image.bytes)?.extension();
                        let path = data_dir.join("attachments").join(format!("sent-{sent_at}-preview.{ext}"));
                        std::fs::write(&path, &*image.bytes).ok()?;
                        Some(AttachmentMeta {
                            content_type: image.content_type.into(),
                            file_name: None,
                            size: image.bytes.len() as u64,
                            width: Some(image.width),
                            height: Some(image.height),
                            local_path: path.to_str().map(Into::into),
                            sticker: false,
                            gif: false,
                            voice: false,
                            remote: None,
                            thumbnail_path: None,
                        })
                    });
                    LinkPreview { url: draft.url, title: Some(draft.title), description: draft.description, date: draft.date, image }
                });
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body,
                    status: DeliveryStatus::Sending,
                    quote,
                    attachments: vec![],
                    link_preview,
                    expires_in: 0,
                };
                let Ok(id) = db.insert_message(&msg).await else { continue };
                let _ = db.set_mentions(id, &spans).await;
                let _ = db.set_styles(id, &styles).await;
                emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                // Walk the status forward like a real send would.
                // Receipts trickle in on their own, without holding up commands.
                let (db, events) = (db.clone(), events.clone());
                tokio::task::spawn_local(async move {
                    // Read receipts off: theirs aren't shown either.
                    let read_receipts = crate::setting_on(db.setting(crate::SETTING_READ_RECEIPTS).await.ok().flatten().as_deref());
                    let walk: &[DeliveryStatus] = if read_receipts {
                        &[DeliveryStatus::Sent, DeliveryStatus::Delivered, DeliveryStatus::Read]
                    } else {
                        &[DeliveryStatus::Sent, DeliveryStatus::Delivered]
                    };
                    for &status in walk {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let _ = db.update_status(&conversation, sent_at, status).await;
                        let _ = events.unbounded_send(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                    }
                });
            }
            Command::React { message_id, emoji, remove } => {
                let result = if remove {
                    db.remove_reaction(message_id, None).await
                } else {
                    db.set_reaction(message_id, Reaction { emoji, author: None, sent_at: now() }).await
                };
                if result.is_ok()
                    && let Ok(Some(m)) = db.message(message_id).await
                {
                    emit(Event::MessageChanged { conversation: m.conversation_id, message_id });
                }
            }
            Command::MarkRead { conversation } => {
                let _ = db.mark_read(&conversation).await;
                let _ = db.take_unread_poll_votes(&conversation).await;
                emit(Event::ConversationsChanged);
            }
            Command::SendPoll { conversation, question, options, allow_multiple } => {
                if signal_store::poll_send_error(&question, &options).is_some() {
                    continue;
                }
                let options: Vec<String> = options.iter().map(|o| o.trim().to_owned()).filter(|o| !o.is_empty()).collect();
                let sent_at = next_sent_at();
                let question = question.trim().to_owned();
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body: question.clone(),
                    status: DeliveryStatus::Sent,
                    quote: None,
                    attachments: vec![],
                    link_preview: None,
                    expires_in: 0,
                };
                let Ok(id) = db.insert_message(&msg).await else { continue };
                let members = db.conversation(&conversation).await.ok().flatten().map(|c| c.members).unwrap_or_default();
                let poll = signal_store::Poll { reached: Some(members.iter().cloned().collect()), ..signal_store::Poll::new(question, options, allow_multiple) };
                let _ = db.set_poll(id, &poll).await;
                emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                emit(Event::ConversationsChanged);
                // A moment later the first member votes for the first
                // option: unread until the chat is read, as Desktop's.
                let Some(voter) = members.first().cloned() else { continue };
                let (db, events) = (db.clone(), events.clone());
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let vote = signal_store::PollVote { author: Some(voter.clone()), options: vec![0], vote_count: 1, sent_at: now(), ..Default::default() };
                    if let Ok(true) = db.update_poll_unread(id, |poll| poll.apply_vote(vote, signal_store::VoteSource::Other)).await {
                        let sender = db.conversation(&ConversationId::direct(&voter)).await.ok().flatten().map(|c| c.title).unwrap_or_default();
                        let _ = events.unbounded_send(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                        let _ = events.unbounded_send(Event::ConversationsChanged);
                        let _ = events.unbounded_send(Event::PollActivity { conversation, message_id: id, sender, ended: false });
                    }
                });
            }
            // A vote shows as sending for a moment (long enough for the
            // spinner), then counts.
            Command::VotePoll { message_id, options } => {
                let mut count = 0;
                let voted = db
                    .update_poll(message_id, |poll| {
                        count = poll.next_vote_count();
                        let vote = signal_store::PollVote { author: None, options, vote_count: count, sent_at: now(), pending: true, ..Default::default() };
                        poll.apply_vote(vote, signal_store::VoteSource::ThisDevice)
                    })
                    .await;
                let Ok(Some(m)) = db.message(message_id).await else { continue };
                emit(Event::MessageChanged { conversation: m.conversation_id.clone(), message_id });
                if let Ok(true) = voted {
                    let (db, events) = (db.clone(), events.clone());
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(DEMO_VOTE_SENDING).await;
                        if let Ok(true) = db.update_poll(message_id, |poll| poll.record_vote_sends(count, &[], &[])).await {
                            let _ = events.unbounded_send(Event::MessageChanged { conversation: m.conversation_id, message_id });
                        }
                    });
                }
            }
            Command::EndPoll { message_id } => {
                let Ok(Some(m)) = db.message(message_id).await else { continue };
                if m.direction != Direction::Outgoing || !matches!(db.update_poll(message_id, signal_store::Poll::end).await, Ok(true)) {
                    continue;
                }
                let question = m.poll.map(|p| p.question).unwrap_or_default();
                let notice = signal_store::Notice::PollEnded { by: None, question, sent_at: m.sent_at };
                if let Ok(Some(id)) = db.insert_notice(&m.conversation_id, next_sent_at(), &notice).await {
                    emit(Event::MessageChanged { conversation: m.conversation_id.clone(), message_id: id });
                }
                emit(Event::MessageChanged { conversation: m.conversation_id, message_id });
            }
            // As the real backend does (`actor/view_once.rs`), without
            // receipts or syncs: a copy for the viewer, the message erased.
            Command::OpenViewOnce { message_id } => {
                let Ok(Some(m)) = db.message(message_id).await else { continue };
                let Some(attachment) = m.attachments.first().cloned() else { continue };
                let (Some(signal_store::ViewOnce::Ready), Direction::Incoming, Some(local)) =
                    (m.view_once, m.direction, attachment.local_path.clone())
                else {
                    continue;
                };
                let dir = data_dir.join(crate::actor::VIEW_ONCE_DIR);
                let ext = Path::new(&local).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                let copy = dir.join(format!("{}.{ext}", next_sent_at()));
                if std::fs::create_dir_all(&dir).and_then(|()| std::fs::copy(&local, &copy)).is_err() {
                    continue;
                }
                match db.erase_view_once(message_id, signal_store::ViewOnce::Viewed).await {
                    Ok(Some(erased)) => {
                        for file in erased.files {
                            let _ = std::fs::remove_file(file);
                        }
                        emit(Event::MessageChanged { conversation: m.conversation_id, message_id });
                        emit(Event::ConversationsChanged);
                        emit(Event::ViewOnceOpened(crate::OpenedViewOnce {
                            message_id,
                            path: copy,
                            content_type: attachment.content_type,
                            size: attachment.width.zip(attachment.height),
                        }));
                    }
                    _ => {
                        let _ = std::fs::remove_file(&copy);
                    }
                }
            }
            // Sent at once, so erased at once, as ours are once sent.
            Command::SendViewOnce { conversation, .. } => {
                let sent_at = next_sent_at();
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body: String::new(),
                    status: DeliveryStatus::Sent,
                    quote: None,
                    attachments: Vec::new(),
                    link_preview: None,
                    expires_in: 0,
                };
                if let Ok(id) = db.insert_message_view_once(&msg, Some(signal_store::ViewOnce::Gone)).await {
                    emit(Event::MessageChanged { conversation, message_id: id });
                }
            }
            Command::DeleteForEveryone { message_id } => {
                // Someone else's message can only be ours to delete as a
                // group admin (we are one in the house group).
                let Ok(Some(m)) = db.message(message_id).await else { continue };
                let deleted = if m.direction == Direction::Incoming {
                    db.mark_deleted_by_admin(message_id, &signal_store::DeletedByAdmin { by: None }).await.is_ok()
                } else {
                    db.mark_deleted(message_id).await.is_ok()
                };
                if deleted {
                    emit(Event::MessageChanged { conversation: m.conversation_id, message_id });
                }
            }
            Command::DeleteForMe { message_ids } => {
                for message_id in message_ids {
                    if let Ok(Some(conversation)) = db.remove_message(message_id).await {
                        emit(Event::MessageRemoved { conversation, message_id });
                    }
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::PinMessage { message_id, duration } => {
                if let Ok(Some(m)) = db.message(message_id).await {
                    let at = now();
                    let expires = duration.map(|s| at + i64::from(s) * 1000);
                    let _ = db.pin_message(&m.conversation_id, message_id, at, expires, crate::remote_config().pinned_message_limit()).await;
                    let notice = signal_store::Notice::Pinned { by: None, author: m.author.clone(), sent_at: m.sent_at };
                    if let Ok(Some(id)) = db.insert_notice(&m.conversation_id, at, &notice).await {
                        emit(Event::MessageChanged { conversation: m.conversation_id.clone(), message_id: id });
                    }
                    emit(Event::PinsChanged(m.conversation_id));
                }
            }
            Command::UnpinMessage { message_id } => {
                if let Ok(Some(m)) = db.message(message_id).await
                    && db.unpin_message(&m.conversation_id, message_id).await.is_ok()
                {
                    emit(Event::PinsChanged(m.conversation_id));
                }
            }
            Command::EditMessage { message_id, body, mentions, styles } => {
                let (body, spans, styles) = demo_mentions(&db, body.trim_end(), &mentions, &styles).await;
                if db.edit_message(message_id, &body, next_sent_at()).await.is_ok()
                    && db.set_mentions(message_id, &spans).await.is_ok()
                    && db.set_styles(message_id, &styles).await.is_ok()
                    && let Ok(Some(m)) = db.message(message_id).await
                {
                    emit(Event::MessageChanged { conversation: m.conversation_id, message_id });
                }
            }
            // Nothing fails offline.
            Command::RetrySend { .. } => {}
            Command::Forward { message_ids, conversations } => {
                for conversation in conversations {
                    for message_id in &message_ids {
                        let Ok(Some(original)) = db.message(*message_id).await else { continue };
                        let sent_at = next_sent_at();
                        let copy = NewMessage {
                            conversation_id: conversation.clone(),
                            sent_at,
                            received_at: sent_at,
                            direction: Direction::Outgoing,
                            author: None,
                            author_name: None,
                            body: original.body.clone(),
                            status: DeliveryStatus::Sent,
                            quote: None,
                            attachments: original.attachments.clone(),
                            link_preview: original.link_preview.clone(),
                            expires_in: 0,
                        };
                        if let Ok(id) = db.insert_message(&copy).await {
                            emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                        }
                    }
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::DeleteConversation { conversation } => {
                if db.remove_conversation(&conversation).await.is_ok() {
                    emit(Event::ConversationRemoved(conversation));
                    let _ = events.unbounded_send(Event::ConversationsChanged);
                }
            }
            Command::AnswerMessageRequest { conversation, answer } => {
                for event in demo_answer_message_request(&db, &conversation, answer).await {
                    emit(event);
                }
            }
            Command::SetExpireTimer { conversation, seconds } => {
                let _ = db.set_expire_timer(&conversation, seconds, 1).await;
                let notice = signal_store::Notice::Timer { by: None, seconds };
                if let Ok(Some(id)) = db.insert_notice(&conversation, now(), &notice).await {
                    emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::UpdateGroup { conversation, title, description: _ } => {
                if let Some(title) = title {
                    let update = signal_store::ConversationUpdate { title: Some(title.clone()), ..Default::default() };
                    let _ = db.upsert_conversation(&conversation, update).await;
                    let notice = signal_store::Notice::Group { by: None, change: signal_store::GroupNotice::Title { title } };
                    if let Ok(Some(id)) = db.insert_notice(&conversation, now(), &notice).await {
                        emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                    }
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::SetMemberLabel { conversation, emoji, text } => {
                if let Ok(Some(chat)) = db.conversation(&conversation).await {
                    let mut labels = chat.member_labels.clone();
                    let me = SELF.trim_start_matches("d:").to_owned();
                    if text.trim().is_empty() {
                        labels.remove(&me);
                    } else {
                        labels.insert(me, signal_store::MemberLabel { emoji, text: text.trim().to_owned() });
                    }
                    let update = signal_store::ConversationUpdate { member_labels: Some(labels), ..Default::default() };
                    let _ = db.upsert_conversation(&conversation, update).await;
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::CreateGroup { title, members, avatar, expire_timer } => {
                let key: [u8; 32] = rand::Rng::random(&mut rand::rng());
                let conversation = ConversationId::group(&key);
                let update = ConversationUpdate {
                    kind: Some(ConversationKind::Group),
                    title: Some(title.trim().to_owned()),
                    expire_timer: Some(expire_timer),
                    members: Some(members),
                    admins: Some(vec![SELF.trim_start_matches("d:").into()]),
                    can_label: Some(true),
                    can_add_members: Some(true),
                    ..Default::default()
                };
                let _ = db.upsert_conversation(&conversation, update).await;
                // The picked photo, as the group's (`media::group_avatar`).
                if let Some(photo) = avatar.and_then(|image| crate::media::group_avatar(&image).ok()) {
                    let file = data_dir.join("avatars").join(format!("demo-{}.jpg", conversation.0.replace(':', "-")));
                    if std::fs::create_dir_all(data_dir.join("avatars")).and_then(|_| std::fs::write(&file, photo)).is_ok() {
                        let _ = db.set_avatar(&conversation, file.to_str()).await;
                    }
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
                emit(Event::GroupCreated(conversation));
            }
            Command::AddGroupMembers { conversation, members } => {
                if let Ok(Some(chat)) = db.conversation(&conversation).await {
                    let mut all = chat.members.clone();
                    for member in members {
                        if !all.contains(&member) {
                            all.push(member.clone());
                            let notice = signal_store::Notice::Group {
                                by: None,
                                change: signal_store::GroupNotice::Added { member: Some(member) },
                            };
                            if let Ok(Some(id)) = db.insert_notice(&conversation, now(), &notice).await {
                                emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                            }
                        }
                    }
                    let update = ConversationUpdate { members: Some(all), ..Default::default() };
                    let _ = db.upsert_conversation(&conversation, update).await;
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::EndGroup { conversation } => {
                let update = ConversationUpdate {
                    ended: Some(true),
                    can_edit_info: Some(false),
                    can_add_members: Some(false),
                    can_label: Some(false),
                    ..Default::default()
                };
                let _ = db.upsert_conversation(&conversation, update).await;
                let notice = signal_store::Notice::Group { by: None, change: signal_store::GroupNotice::Ended };
                if let Ok(Some(id)) = db.insert_notice(&conversation, now(), &notice).await {
                    emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::LeaveGroup { conversation } => {
                let _ = db.set_left(&conversation, true).await;
                let notice = signal_store::Notice::Group { by: None, change: signal_store::GroupNotice::Removed { member: None } };
                if let Ok(Some(id)) = db.insert_notice(&conversation, now(), &notice).await {
                    emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::SetChat { conversation, change } => {
                let _ = match change {
                    crate::ChatChange::Archived(on) => db.set_archived(&conversation, on).await,
                    crate::ChatChange::MutedUntil(until) => db.set_muted_until(&conversation, until).await,
                    crate::ChatChange::MarkedUnread(on) => db.set_marked_unread(&conversation, on).await,
                    crate::ChatChange::Blocked(on) => db.set_blocked(&conversation, on).await,
                    crate::ChatChange::HideStory(on) => {
                        let done = db.set_story_hidden(&conversation, on).await;
                        let _ = events.unbounded_send(Event::StorySettingsChanged);
                        done
                    }
                };
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::SetNickname { conversation, nickname } => {
                let Some(sid) = conversation.as_direct().filter(|_| conversation.0 != SELF).map(str::to_owned) else { continue };
                let Ok(name) = db.set_contact_nickname(&sid, &signal_store::limit_nickname(&nickname)).await else { continue };
                let phone = db.conversation(&conversation).await.ok().flatten().and_then(|c| c.phone);
                let Some(title) = name.or(phone) else { continue };
                let update = ConversationUpdate { title: Some(title.clone()), ..Default::default() };
                let _ = db.upsert_conversation(&conversation, update).await;
                let _ = db.retitle_directory_entry(&conversation, &title).await;
                let names = vec![(sid, title)];
                if let Ok(renamed) = db.rename_authors(&names).await
                    && renamed > 0
                {
                    emit(Event::AuthorNamesChanged(names));
                }
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            Command::SetPinned { conversation, pinned } => {
                set_pinned(&db, conversation, pinned).await;
                let _ = events.unbounded_send(Event::ConversationsChanged);
            }
            // Optimized for real and kept locally; nothing leaves the machine.
            // Real searches when you've added a Giphy key; sends stay local.
            Command::SearchGifs { query, offset } => {
                let (db, events, cache) = (db.clone(), events.clone(), data_dir.join("gif-cache"));
                tokio::task::spawn_local(async move {
                    let result = crate::search_gifs(&db, &cache, &query, offset).await;
                    let _ = events.unbounded_send(Event::Gifs { query, offset, result });
                });
            }
            Command::SendGif { conversation, gif, quote } => {
                let bytes = match crate::giphy::download(&gif.original.url).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        emit(Event::Error(format!("couldn't get the GIF: {error:#}")));
                        continue;
                    }
                };
                let path = data_dir.join("attachments").join(format!("sent-{}.mp4", now()));
                if std::fs::write(&path, &bytes).is_err() {
                    continue;
                }
                let quote = demo_quote(&db, quote).await;
                let sent_at = next_sent_at();
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body: String::new(),
                    status: DeliveryStatus::Sent,
                    quote,
                    attachments: vec![AttachmentMeta {
                        content_type: "video/mp4".into(),
                        file_name: None,
                        size: bytes.len() as u64,
                        width: Some(gif.original.width),
                        height: Some(gif.original.height),
                        local_path: path.to_str().map(Into::into),
                        sticker: false,
                        gif: true,
                        voice: false,
                        remote: None,
                        thumbnail_path: None,
                    }],
                    link_preview: None,
                    expires_in: 0,
                };
                if let Ok(id) = db.insert_message(&msg).await {
                    emit(Event::MessageChanged { conversation, message_id: id });
                }
            }
            Command::LoadStickers => emit(Event::Stickers(demo_stickers(&data_dir))),
            // Pretends to upload, a sticker at a time; nothing is installed.
            Command::UploadStickerPack(pack) => {
                use crate::sticker_upload::{StickerUpload, share_url};
                let events = events.clone();
                tokio::task::spawn_local(async move {
                    let total = pack.stickers.len();
                    for done in 1..=total {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        let _ = events.unbounded_send(Event::StickerUpload(StickerUpload::Progress { done, total }));
                    }
                    let (pack_id, pack_key) = (vec![0xde, 0x71], vec![0; 32]);
                    let url = share_url(&pack_id, &pack_key);
                    let _ = events.unbounded_send(Event::StickerUpload(StickerUpload::Done { pack_id, pack_key, url }));
                });
            }
            Command::SendSticker { conversation, pack: _, sticker, quote } => {
                let quote = demo_quote(&db, quote).await;
                let sent_at = next_sent_at();
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body: String::new(),
                    status: DeliveryStatus::Sent,
                    quote,
                    attachments: vec![AttachmentMeta {
                        content_type: "image/png".into(),
                        file_name: None,
                        size: 0,
                        width: Some(STICKER_SIZE),
                        height: Some(STICKER_SIZE),
                        local_path: sticker.path.to_str().map(Into::into),
                        sticker: true,
                        gif: false,
                        voice: false,
                        remote: None,
                        thumbnail_path: None,
                    }],
                    link_preview: None,
                    expires_in: 0,
                };
                if let Ok(id) = db.insert_message(&msg).await {
                    emit(Event::MessageChanged { conversation, message_id: id });
                }
            }
            // No servers to ask: say so.
            Command::FindRecipient { query } => {
                emit(Event::RecipientFound { query, result: Err("Looking people up isn't available in the demo".into()) });
            }
            Command::SendImages { conversation, images, caption, quote, mentions, styles } => {
                let mut attachments = Vec::new();
                for (ix, image) in images.into_iter().enumerate() {
                    let bytes = image.bytes.as_ref().clone();
                    let level = crate::media::Optimization::from_setting(
                        db.setting(crate::SETTING_IMAGE_OPTIMIZATION).await.ok().flatten().as_deref(),
                    );
                    let prepared = tokio::task::spawn_blocking(move || {
                        crate::media::prepare(&crate::media::normalize(bytes)?, level)
                    })
                    .await;
                    let prepared = match prepared {
                        Ok(Ok(prepared)) => prepared,
                        Ok(Err(error)) => {
                            emit(Event::Error(format!("can't send image: {error:#}")));
                            continue;
                        }
                        Err(_) => continue,
                    };
                    tracing::info!(before = image.bytes.len(), after = prepared.bytes.len(), "optimized image");
                    let path = data_dir.join("attachments").join(format!("sent-{}-{ix}.{}", now(), prepared.extension));
                    if std::fs::write(&path, &prepared.bytes).is_err() {
                        continue;
                    }
                    attachments.push(AttachmentMeta {
                        content_type: prepared.content_type.into(),
                        file_name: image.file_name,
                        size: prepared.bytes.len() as u64,
                        width: Some(prepared.width),
                        height: Some(prepared.height),
                        local_path: path.to_str().map(Into::into),
                        sticker: false,
                        gif: false,
                        voice: false,
                        remote: None,
                        thumbnail_path: None,
                    });
                }
                if attachments.is_empty() {
                    continue;
                }
                let sent_at = next_sent_at();
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body: String::new(),
                    status: DeliveryStatus::Sent,
                    quote: demo_quote(&db, quote).await,
                    attachments,
                    link_preview: None,
                    expires_in: 0,
                };
                let (caption, spans, styles) = demo_mentions(&db, caption.trim_end(), &mentions, &styles).await;
                let msg = NewMessage { body: caption, ..msg };
                if let Ok(id) = db.insert_message(&msg).await {
                    let _ = db.set_mentions(id, &spans).await;
                    let _ = db.set_styles(id, &styles).await;
                    emit(Event::MessageChanged { conversation, message_id: id });
                }
            }
            Command::SendFile { conversation, file, caption, quote, mentions, styles } => {
                let name = crate::media::safe_file_name(&file.file_name);
                let dir = data_dir.join("attachments").join(format!("sent-{}", now()));
                let path = dir.join(&name);
                if std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, file.bytes.as_slice())).is_err() {
                    continue;
                }
                let sent_at = next_sent_at();
                let (body, spans, styles) = demo_mentions(&db, caption.trim_end(), &mentions, &styles).await;
                let msg = NewMessage {
                    conversation_id: conversation.clone(),
                    sent_at,
                    received_at: sent_at,
                    direction: Direction::Outgoing,
                    author: None,
                    author_name: None,
                    body,
                    status: DeliveryStatus::Sent,
                    quote: demo_quote(&db, quote).await,
                    attachments: vec![AttachmentMeta {
                        content_type: if file.voice {
                            crate::VOICE_CONTENT_TYPE.into()
                        } else {
                            crate::media::content_type_for_file(&name).into()
                        },
                        file_name: (!file.voice).then_some(name),
                        size: file.bytes.len() as u64,
                        width: None,
                        height: None,
                        local_path: path.to_str().map(Into::into),
                        sticker: false,
                        gif: false,
                        voice: file.voice,
                        remote: None,
                        thumbnail_path: None,
                    }],
                    link_preview: None,
                    expires_in: 0,
                };
                if let Ok(id) = db.insert_message(&msg).await {
                    let _ = db.set_mentions(id, &spans).await;
                    let _ = db.set_styles(id, &styles).await;
                    emit(Event::MessageChanged { conversation, message_id: id });
                }
            }
            // Made-up digits (there are no keys here), stable per chat.
            Command::LoadSafetyNumber { conversation } => {
                let verified = db.setting(&format!("demo.verified.{}", conversation.0)).await.ok().flatten().is_some_and(|v| v == "1");
                let auto_verify = demo_auto_verify(&db, &conversation).await;
                emit(Event::SafetyNumber { conversation: conversation.clone(), result: Ok(demo_safety_number(&conversation, verified, auto_verify)) });
            }
            Command::ViewStory { id } => {
                if let Ok(Some(story)) = db.story(id).await
                    && db.mark_story_read(story.author.as_deref(), story.sent_at).await.unwrap_or(false)
                {
                    emit(Event::StoriesChanged);
                }
            }
            // Demo stories are here already.
            Command::FetchStory { .. } => {}
            // A group story's reply shows with it; one to one, it goes nowhere.
            Command::ReplyToStory { id, body, emoji } => {
                if let Ok(Some(story)) = db.story(id).await
                    && story.group.is_some()
                {
                    let body = emoji.clone().unwrap_or_else(|| body.trim().to_owned());
                    let reply = signal_store::StoryReply {
                        author: None,
                        author_name: None,
                        sent_at: next_sent_at(),
                        body,
                        emoji,
                        deleted: false,
                    };
                    if db.add_story_reply(story.author.as_deref(), story.sent_at, &reply).await.is_ok() {
                        emit(Event::StoriesChanged);
                    }
                }
            }
            // A posted story is ours here at once; nothing is sent. Story
            // lists share one story (and timestamp), as in `post_story`.
            Command::PostStory { story, targets } => {
                let now = now();
                let all_lists = demo_story_lists(&db).await;
                let mut recipients: Vec<signal_store::StoryRecipient> = Vec::new();
                let mut groups = Vec::new();
                for target in &targets {
                    let list = match target {
                        crate::StoryTarget::MyStory => {
                            all_lists.iter().find(|l| l.is_my_story()).cloned().unwrap_or_else(crate::StoryList::my_story)
                        }
                        crate::StoryTarget::List(id) => match all_lists.iter().find(|l| &l.id == id) {
                            Some(list) => list.clone(),
                            None => continue,
                        },
                        crate::StoryTarget::Group(group) => {
                            groups.push(group.clone());
                            continue;
                        }
                    };
                    for member in demo_list_members(&db, &list).await {
                        match recipients.iter_mut().find(|r| r.service_id == member) {
                            Some(r) => {
                                r.list_ids.push(list.id.clone());
                                r.allows_replies |= list.allows_replies;
                            }
                            None => recipients.push(signal_store::StoryRecipient {
                                service_id: member,
                                list_ids: vec![list.id.clone()],
                                allows_replies: list.allows_replies,
                            }),
                        }
                    }
                }
                // Groups posted to stay on the list, as in `post_story`.
                let modes: Vec<_> = groups.iter().map(|g| (g.clone(), signal_store::StorySendMode::Always)).collect();
                if db.set_story_send_modes(&modes).await.unwrap_or(false) {
                    emit(Event::StorySettingsChanged);
                }
                let to_lists = targets.iter().any(|t| !matches!(t, crate::StoryTarget::Group(_)));
                let rows = to_lists.then_some(None).into_iter().chain(groups.into_iter().map(Some));
                for (ix, group) in rows.enumerate() {
                    let content = match &story {
                        crate::OutgoingStory::Text { story: text, preview } => {
                            let mut text = (**text).clone();
                            text.preview = preview.as_ref().map(|p| signal_store::LinkPreview {
                                url: p.url.clone(),
                                title: Some(p.title.clone()),
                                description: p.description.clone(),
                                date: p.date,
                                image: None,
                            });
                            signal_store::StoryContent::Text(text)
                        }
                        crate::OutgoingStory::Media { bytes, content_type, caption, styles } => {
                            let path = data_dir.join("attachments").join(format!("story-sent-{now}-{ix}"));
                            if std::fs::write(&path, bytes.as_slice()).is_err() {
                                continue;
                            }
                            signal_store::StoryContent::Media {
                                attachment: AttachmentMeta {
                                    content_type: content_type.clone(),
                                    file_name: None,
                                    size: bytes.len() as u64,
                                    width: None,
                                    height: None,
                                    local_path: path.to_str().map(Into::into),
                                    sticker: false,
                                    gif: false,
                                    voice: false,
                                    remote: None,
                                    thumbnail_path: None,
                                },
                                caption: caption.clone(),
                                styles: styles.clone(),
                            }
                        }
                    };
                    let story = signal_store::NewStory {
                        author: None,
                        author_name: None,
                        sent_at: now + ix as i64,
                        received_at: now,
                        recipients: if group.is_none() { recipients.clone() } else { Vec::new() },
                        group,
                        content,
                        pointer: None,
                        allows_replies: true,
                        read: true,
                    };
                    let _ = db.insert_story(&story, now).await;
                }
                emit(Event::StoriesChanged);
            }
            // Story settings are kept here only.
            Command::SaveStoryList(list) => {
                let mut lists = demo_story_lists(&db).await;
                match lists.iter_mut().find(|l| l.id == list.id) {
                    Some(existing) => *existing = list.clone(),
                    None => lists.push(list.clone()),
                }
                if let Ok(json) = serde_json::to_string(&lists) {
                    let _ = db.set_setting(crate::SETTING_STORY_LISTS, &json).await;
                }
                if list.is_my_story() {
                    let _ = db.set_setting(crate::SETTING_MY_STORY_PRIVACY_SET, "true").await;
                }
                emit(Event::StorySettingsChanged);
            }
            Command::DeleteStoryList { id } => {
                let mut lists = demo_story_lists(&db).await;
                lists.retain(|l| l.id != id || l.is_my_story());
                if let Ok(json) = serde_json::to_string(&lists) {
                    let _ = db.set_setting(crate::SETTING_STORY_LISTS, &json).await;
                }
                emit(Event::StorySettingsChanged);
            }
            Command::SetStoriesDisabled(disabled) => {
                let _ = db.set_setting(crate::SETTING_STORIES_DISABLED, if disabled { "true" } else { "false" }).await;
                if disabled && let Ok(files) = db.remove_all_stories().await {
                    for file in files {
                        let _ = std::fs::remove_file(file);
                    }
                }
                emit(Event::StorySettingsChanged);
                emit(Event::StoriesChanged);
            }
            Command::SetAccountSetting { setting, on } => {
                let _ = db.set_setting(setting.key(), if on { "true" } else { "false" }).await;
                emit(Event::AccountSettingsChanged);
            }
            Command::SetStoryViewReceipts(on) => {
                let _ = db.set_setting(crate::SETTING_STORY_VIEW_RECEIPTS, if on { "true" } else { "false" }).await;
                emit(Event::StorySettingsChanged);
            }
            Command::SetStorySendMode { groups, mode } => {
                let modes: Vec<_> = groups.into_iter().map(|g| (g, mode)).collect();
                if db.set_story_send_modes(&modes).await.unwrap_or(false) {
                    emit(Event::StorySettingsChanged);
                }
            }
            // Gone here; there's no one to tell.
            Command::DeleteStory { id } => {
                if let Ok(Some(story)) = db.story(id).await
                    && story.author.is_none()
                    && let Ok(file) = db.remove_story(id).await
                {
                    if let Some(file) = file {
                        let _ = std::fs::remove_file(file);
                    }
                    emit(Event::StoriesChanged);
                }
            }
            Command::DeleteStoryReply { story, author, sent_at, for_everyone } => {
                let for_everyone = for_everyone && author.is_none();
                if db.remove_story_reply(story, author.as_deref(), sent_at, for_everyone).await.unwrap_or(false) {
                    emit(Event::StoriesChanged);
                }
            }
            Command::PressMegaphone { id, .. } => {
                if megaphone.as_ref().is_some_and(|m| m.id == id) {
                    megaphone = None;
                    emit(Event::Megaphones(Vec::new()));
                }
            }
            Command::SetVerified { conversation, verified } => {
                let _ = db.set_setting(&format!("demo.verified.{}", conversation.0), if verified { "1" } else { "" }).await;
                let auto_verify = demo_auto_verify(&db, &conversation).await;
                emit(Event::SafetyNumber { conversation: conversation.clone(), result: Ok(demo_safety_number(&conversation, verified, auto_verify)) });
            }
            Command::MarkCallsRead { conversation } => {
                let call = match &conversation {
                    Some(conversation) => db.latest_unseen_call(conversation).await,
                    None => db.latest_call(None).await,
                };
                if let Ok(Some(call)) = call {
                    let target = signal_store::CallTarget { conversation: Some(call.conversation), call_id: call.call_id, timestamp: call.sent_at };
                    for (conversation, message_id) in db.mark_calls_seen(&target, conversation.is_some(), now(), None).await.unwrap_or_default() {
                        emit(Event::MessageChanged { conversation, message_id });
                    }
                    emit(Event::ConversationsChanged);
                }
            }
            Command::ClearCallHistory => {
                if let Ok(Some(call)) = db.latest_call(None).await {
                    let target = signal_store::CallTarget { conversation: Some(call.conversation), call_id: call.call_id, timestamp: call.sent_at };
                    for (conversation, message_id) in db.clear_call_history(&target).await.unwrap_or_default() {
                        emit(Event::MessageRemoved { conversation, message_id });
                    }
                    emit(Event::ConversationsChanged);
                }
                // Links' calls go, links we joined go, and ours are deleted.
                let _ = db.clear_adhoc_calls(now()).await;
                for link in db.call_links().await.unwrap_or_default() {
                    let _ = if link.is_admin() {
                        db.mark_call_link_deleted(&link.room_id, now()).await.map(|_| ())
                    } else {
                        db.remove_call_link(&link.room_id).await
                    };
                }
                emit(Event::CallLinksChanged);
            }
            // Made up, and nothing is sent: Maya's key verifies after a
            // moment, anyone else's doesn't.
            Command::CheckKeyTransparency { conversation } => {
                let events = events.clone();
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    let verified = conversation.0 == MAYA;
                    let _ = events.unbounded_send(Event::KeyTransparency { conversation, verified });
                });
            }
            Command::SetKeyTransparencyDisabled(disabled) => {
                let _ = db.set_setting(crate::SETTING_KEY_TRANSPARENCY_DISABLED, if disabled { "true" } else { "false" }).await;
                emit(Event::StorySettingsChanged);
            }
            Command::CancelKeyTransparency => {}
            // Group administration is `demo_groups::handle`'s.
            Command::SetGroupAvatar { .. }
            | Command::RemoveFromGroup { .. }
            | Command::SetGroupAdmin { .. }
            | Command::SetGroupPermission { .. }
            | Command::ChangeGroupLink { .. }
            | Command::AnswerJoinRequest { .. }
            | Command::RevokeGroupInvites { .. }
            | Command::CancelJoinRequest { .. }
            | Command::OpenGroupLink { .. }
            | Command::JoinGroupLink { .. }
            | Command::RefreshGroup { .. }
            | Command::Link
            | Command::Typing { .. }
            | Command::FetchAttachment { .. }
            | Command::FetchLongText { .. }
            | Command::Unlink
            | Command::SkipHistory
            | Command::RefreshRemoteConfig => {}
        }
    }
}

fn demo_safety_number(conversation: &ConversationId, verified: bool, auto_verify: crate::AutoVerify) -> crate::SafetyNumber {
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(conversation.0.as_bytes());
    let digits: String = hash.iter().cycle().take(60).map(|b| char::from(b'0' + b % 10)).collect();
    crate::SafetyNumber { qr: digits.clone().into_bytes(), digits, verified, auto_verify }
}

/// As the real backend decides it: off by the remote config or in
/// Settings, or whether we have their number.
async fn demo_auto_verify(db: &AppDb, conversation: &ConversationId) -> crate::AutoVerify {
    if !crate::key_transparency::feature_on()
        || db.setting(crate::SETTING_KEY_TRANSPARENCY_DISABLED).await.ok().flatten().as_deref() == Some("true") {
        return crate::AutoVerify::Off;
    }
    match db.conversation(conversation).await.ok().flatten().and_then(|c| c.phone) {
        Some(_) => crate::AutoVerify::Available,
        None => crate::AutoVerify::Unavailable,
    }
}

/// The real backend's answer to a message request, without a network:
/// the answer, its notices, the block, profile sharing, the report flag,
/// deleting, and joining, declining or leaving a group.
async fn demo_answer_message_request(
    db: &AppDb,
    conversation: &ConversationId,
    answer: signal_store::MessageRequestResponse,
) -> Vec<Event> {
    use signal_store::{GroupNotice, MessageRequestResponse as Answer, Notice};
    let Ok(Some(before)) = db.conversation(conversation).await else { return Vec::new() };
    let group = before.kind == ConversationKind::Group;
    let mut events = Vec::new();
    let mut notices = Vec::new();
    let at = next_sent_at();
    if answer == Answer::Accept && before.invited() {
        notices.extend(crate::demo_groups::accept_invite(db, conversation).await);
    }
    let _ = db.set_message_request(conversation, answer).await;
    let who = conversation.as_direct().map(str::to_owned);
    for event in crate::message_requests::answer_events(before.message_request, before.blocked, answer) {
        notices.push(Notice::MessageRequest { event, group, who: who.clone() });
    }
    if answer == Answer::Accept {
        let _ = db.set_blocked(conversation, false).await;
        let _ = db.set_profile_sharing(conversation, true).await;
    } else {
        if answer.blocks() {
            let _ = db.set_blocked(conversation, true).await;
        }
        if answer.blocks() || answer.deletes() {
            let _ = db.set_profile_sharing(conversation, false).await;
            if group && !before.left {
                if before.invited() {
                    notices.push(crate::demo_groups::decline_invite(db, conversation).await);
                } else {
                    let _ = db.set_left(conversation, true).await;
                    notices.push(Notice::Group { by: None, change: GroupNotice::Removed { member: None } });
                }
            }
        }
        if answer.spam() {
            let _ = db.set_reported(conversation).await;
        }
    }
    if answer.deletes() {
        if answer.blocks() || before.blocked {
            let _ = db.clear_messages(conversation).await;
            events.push(Event::ConversationCleared(conversation.clone()));
        } else {
            let _ = db.remove_conversation(conversation).await;
            events.push(Event::ConversationRemoved(conversation.clone()));
            notices.clear();
        }
    }
    for (ix, notice) in notices.into_iter().enumerate() {
        if let Ok(Some(id)) = db.insert_notice(conversation, at + ix as i64, &notice).await {
            events.push(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
        }
    }
    events.push(Event::ConversationsChanged);
    events.push(Event::MessageRequestAnswered { conversation: conversation.clone(), answer });
    events
}

/// "@Name" for each mention placeholder, names from the demo's chats.
/// "@Name" for each mention placeholder, and where each mention and style
/// (spans over `body`) lands, as the real backend renders them.
async fn demo_mentions(
    db: &AppDb,
    body: &str,
    mentions: &[String],
    styles: &[signal_store::StyleSpan],
) -> (String, Vec<signal_store::MentionSpan>, Vec<signal_store::StyleSpan>) {
    let mut names = Vec::new();
    for aci in mentions {
        let chat = db.conversation(&ConversationId::direct(aci)).await.ok().flatten();
        names.push(chat.map_or_else(|| "Unknown".to_owned(), |c| c.title));
    }
    crate::mapping::render_body(body, mentions, &names, &crate::mapping::wire_styles(body, styles))
}

/// Six seconds of tones that rise and fall like speech (8 kHz mono WAV),
/// for the demo's voice message.
fn demo_voice_wav() -> Vec<u8> {
    const RATE: u32 = 8000;
    let samples: Vec<i16> = (0..RATE * 6)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            // Syllables: a loudness that swells and dips a few times a second.
            let envelope = ((t * 2.7).sin() * (t * 0.9).sin()).abs().powf(0.7);
            let tone = (t * std::f32::consts::TAU * (180. + 40. * (t * 1.3).sin())).sin();
            (tone * envelope * 12_000.) as i16
        })
        .collect();
    let data_len = samples.len() as u32 * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// The quote for a reply to `quote`, as the real backend stores it.
async fn demo_quote(db: &AppDb, quote: Option<i64>) -> Option<Quote> {
    let m = db.message(quote?).await.ok().flatten()?;
    Some(Quote {
        author: m.author.clone(),
        author_name: m.author_name.clone().unwrap_or_default(),
        sent_at: m.sent_at,
        text: m.body.clone(),
        thumbnail: m.thumbnail(),
        attachment: m.attachments.first().map(signal_store::QuotedAttachment::of),
        story: false,
    })
}

/// Local stand-in for the Storage Service pin write.
async fn set_pinned(db: &AppDb, conversation: ConversationId, pinned: bool) {
    let mut pins = db.pinned_ids().await.unwrap_or_default();
    pins.retain(|id| *id != conversation);
    if pinned && pins.len() < crate::remote_config().pinned_chat_limit() {
        pins.push(conversation);
    }
    let _ = db.replace_pinned(&pins).await;
}
