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
const SELF: &str = "d:00000000-0000-4000-8000-000000000001";

const MINUTE: i64 = 60_000;
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
            ..Default::default()
        },
    )
    .await?;
    db.upsert_conversation(
        &ConversationId(HOUSE.into()),
        ConversationUpdate { members: Some(vec![aci(ANA).into(), aci(LEO).into()]), ..Default::default() },
    )
    .await?;

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
        }];
        db.insert_message(&voice).await?;
    }
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
    db.set_muted(&ConversationId(HOUSE.into()), true).await?;

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
    Ok(())
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

pub(crate) async fn run(db: AppDb, data_dir: PathBuf, mut commands: mpsc::UnboundedReceiver<Command>, events: mpsc::UnboundedSender<Event>) {
    let emit = |e: Event| {
        let _ = events.unbounded_send(e);
    };
    if let Err(e) = seed(&db, &data_dir).await {
        emit(Event::Error(format!("seeding demo data: {e}")));
    }
    emit(Event::Ready {
        phone: Some("+1 555 0123 (demo)".into()),
        aci: SELF.trim_start_matches("d:").into(),
    });
    emit(Event::Connection(ConnectionState::Online));
    // A pretend call: (the call, when it started, whether it connected).
    let call: std::rc::Rc<std::cell::RefCell<Option<(crate::CallUpdate, i64, bool)>>> = Default::default();

    while let Some(command) = commands.next().await {
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
                let group = conversation.as_group().is_some();
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
                // A group call's notice is "You started a group call", from the start.
                if update.group {
                    if let Ok(Some(id)) = db.insert_notice(&update.conversation, started, &signal_store::Notice::GroupCall { by: None, era: None }).await {
                        emit(Event::MessageChanged { conversation: update.conversation.clone(), message_id: id });
                    }
                    emit(Event::Call(None));
                    continue;
                }
                let outcome = if connected { signal_store::CallOutcome::Accepted } else { signal_store::CallOutcome::Unanswered };
                let notice = signal_store::Notice::Call { incoming: false, video: update.video, outcome, call_id: None };
                if let Ok(Some(id)) = db.insert_notice(&update.conversation, started, &notice).await {
                    emit(Event::MessageChanged { conversation: update.conversation.clone(), message_id: id });
                }
                emit(Event::Call(None));
            }
            Command::AcceptCall { .. } | Command::SetCallAudio(_) | Command::SetCallVideo(_) => {}
            // Maya's in the hiking group's call; no one's in the others'.
            Command::PeekGroupCall { conversation } => {
                let users = if conversation.0 == GROUP { vec![aci(MAYA).to_owned()] } else { Vec::new() };
                emit(Event::GroupPeek { conversation, users });
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
                tokio::task::spawn_local(async move {
                    let preview = crate::link_preview::fetch(&url, crate::media::Optimization::Max).await.unwrap_or(None);
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
                    for status in [DeliveryStatus::Sent, DeliveryStatus::Delivered, DeliveryStatus::Read] {
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
                emit(Event::ConversationsChanged);
            }
            Command::DeleteForEveryone { message_id } => {
                if db.mark_deleted(message_id).await.is_ok()
                    && let Ok(Some(m)) = db.message(message_id).await
                {
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
                    let _ = db.pin_message(&m.conversation_id, message_id, at, expires, crate::MAX_PINNED_MESSAGES).await;
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
            Command::CreateGroup { title, members } => {
                let key: [u8; 32] = rand::Rng::random(&mut rand::rng());
                let conversation = ConversationId::group(&key);
                let update = ConversationUpdate {
                    kind: Some(ConversationKind::Group),
                    title: Some(title.trim().to_owned()),
                    members: Some(members),
                    admins: Some(vec![SELF.trim_start_matches("d:").into()]),
                    can_label: Some(true),
                    ..Default::default()
                };
                let _ = db.upsert_conversation(&conversation, update).await;
                let _ = events.unbounded_send(Event::ConversationsChanged);
                emit(Event::GroupCreated(conversation));
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
                };
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
            Command::SendGif { conversation, gif } => {
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
            Command::SendSticker { conversation, pack: _, sticker } => {
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
                emit(Event::SafetyNumber { conversation: conversation.clone(), result: Ok(demo_safety_number(&conversation, verified)) });
            }
            Command::SetVerified { conversation, verified } => {
                let _ = db.set_setting(&format!("demo.verified.{}", conversation.0), if verified { "1" } else { "" }).await;
                emit(Event::SafetyNumber { conversation: conversation.clone(), result: Ok(demo_safety_number(&conversation, verified)) });
            }
            Command::Link | Command::Typing { .. } | Command::FetchAttachment { .. } | Command::Unlink | Command::SkipHistory => {}
        }
    }
}

fn demo_safety_number(conversation: &ConversationId, verified: bool) -> crate::SafetyNumber {
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(conversation.0.as_bytes());
    let digits: String = hash.iter().cycle().take(60).map(|b| char::from(b'0' + b % 10)).collect();
    crate::SafetyNumber { qr: digits.clone().into_bytes(), digits, verified }
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
    })
}

/// Local stand-in for the Storage Service pin write.
async fn set_pinned(db: &AppDb, conversation: ConversationId, pinned: bool) {
    let mut pins = db.pinned_ids().await.unwrap_or_default();
    pins.retain(|id| *id != conversation);
    if pinned && pins.len() < crate::storage::MAX_PINNED {
        pins.push(conversation);
    }
    let _ = db.replace_pinned(&pins).await;
}
