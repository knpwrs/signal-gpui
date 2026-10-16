//! Stories (Desktop's `type = 'story'` messages): who posted what, when it
//! goes, who viewed ours, and a group story's replies.

use sqlx::Row as _;

use crate::{AppDb, AttachmentMeta, ConversationId, LinkPreview, Result, StyleSpan};

/// Settings key: chats whose stories are hidden (JSON conversation ids).
pub const HIDDEN_STORIES: &str = "stories_hidden";

/// A story lasts a day (Desktop's `expireTimer = DurationInSeconds.DAY`).
pub const STORY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

/// Signal's `TextAttachment.Style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum StoryFont {
    #[default]
    Default,
    Regular,
    Bold,
    Serif,
    Script,
    Condensed,
}

/// A text story's background (`TextAttachment.background`): ARGB colors.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StoryBackground {
    Color(u32),
    /// `angle` in degrees: 0 runs bottom to top, 90 left to right.
    Gradient { colors: Vec<u32>, positions: Vec<f32>, angle: u32 },
}

impl Default for StoryBackground {
    fn default() -> Self {
        // Desktop's default: white.
        Self::Color(0xffff_ffff)
    }
}

/// A text story (`TextAttachment`).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct TextStory {
    pub text: String,
    pub font: StoryFont,
    /// ARGB; Desktop's default is its `COLOR_WHITE_INT`.
    pub foreground: Option<u32>,
    /// The pill behind the text, if any.
    pub text_background: Option<u32>,
    pub background: StoryBackground,
    pub preview: Option<LinkPreview>,
}

/// What a story shows.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StoryContent {
    Text(TextStory),
    /// A photo or video, with its caption (`StoryMessage.bodyRanges` style
    /// the caption).
    Media { attachment: AttachmentMeta, caption: String, styles: Vec<StyleSpan> },
}

/// A reply or reaction to a group story, shown with it (1:1 replies go to
/// the chat instead).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoryReply {
    /// `None` for ours.
    pub author: Option<String>,
    pub author_name: Option<String>,
    pub sent_at: i64,
    pub body: String,
    /// A reaction rather than a reply.
    pub emoji: Option<String>,
}

/// Someone one of our stories went to (Desktop's `StoryMessageRecipient`):
/// the story lists that sent it to them (none for a group's story), and
/// whether they may reply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoryRecipient {
    pub service_id: String,
    #[serde(default)]
    pub list_ids: Vec<String>,
    #[serde(default)]
    pub allows_replies: bool,
}

/// Someone who viewed one of our stories, and when we heard (unix
/// millis; `None` for views kept before the time was).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoryView {
    pub viewer: String,
    pub at: Option<i64>,
}

impl<'de> serde::Deserialize<'de> for StoryView {
    /// Older rows list viewers as plain strings.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Viewer(String),
            Full { viewer: String, at: Option<i64> },
        }
        Ok(match Stored::deserialize(deserializer)? {
            Stored::Viewer(viewer) => StoryView { viewer, at: None },
            Stored::Full { viewer, at } => StoryView { viewer, at },
        })
    }
}

/// A story as the UI shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Story {
    pub id: i64,
    /// `None` for ours.
    pub author: Option<String>,
    pub author_name: Option<String>,
    pub sent_at: i64,
    pub received_at: i64,
    /// The group it was posted to, if any.
    pub group: Option<ConversationId>,
    pub content: StoryContent,
    pub allows_replies: bool,
    pub read: bool,
    pub expires_at: i64,
    pub replies: Vec<StoryReply>,
    /// Ours: who viewed it, in the order they did.
    pub views: Vec<StoryView>,
    /// Ours: who it went to.
    pub recipients: Vec<StoryRecipient>,
}

/// A story to keep.
#[derive(Debug, Clone, PartialEq)]
pub struct NewStory {
    pub author: Option<String>,
    pub author_name: Option<String>,
    pub sent_at: i64,
    pub received_at: i64,
    pub group: Option<ConversationId>,
    pub content: StoryContent,
    /// The attachment pointer's protobuf bytes, to download it later.
    pub pointer: Option<Vec<u8>>,
    pub allows_replies: bool,
    pub read: bool,
    /// Ours: who it goes to.
    pub recipients: Vec<StoryRecipient>,
}

const COLUMNS: &str = "id, author_key, author_name, sent_at, received_at, group_id, content, allows_replies, is_read, \
                       expires_at, replies, views, recipients";

fn story_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Story> {
    let author: String = row.try_get("author_key")?;
    let json = |column: &str| -> Result<Option<String>> { Ok(row.try_get::<Option<String>, _>(column)?) };
    Ok(Story {
        id: row.try_get("id")?,
        author: Some(author).filter(|a| !a.is_empty()),
        author_name: row.try_get("author_name")?,
        sent_at: row.try_get("sent_at")?,
        received_at: row.try_get("received_at")?,
        group: row.try_get::<Option<String>, _>("group_id")?.map(ConversationId),
        content: serde_json::from_str(row.try_get("content")?)?,
        allows_replies: row.try_get("allows_replies")?,
        read: row.try_get("is_read")?,
        expires_at: row.try_get("expires_at")?,
        replies: json("replies")?.map(|r| serde_json::from_str(&r)).transpose()?.unwrap_or_default(),
        views: json("views")?.map(|v| serde_json::from_str(&v)).transpose()?.unwrap_or_default(),
        recipients: json("recipients")?.map(|r| serde_json::from_str(&r)).transpose()?.unwrap_or_default(),
    })
}

impl AppDb {
    /// Keeps a story, unless it's here already or already gone. Returns its
    /// id when it's new.
    pub async fn insert_story(&self, story: &NewStory, now: i64) -> Result<Option<i64>> {
        let expires_at = story.sent_at + STORY_LIFETIME_MS;
        if expires_at <= now {
            return Ok(None);
        }
        Ok(sqlx::query_scalar(
            "INSERT INTO stories (author_key, author_name, sent_at, received_at, group_id, content, pointer, \
                allows_replies, is_read, expires_at, recipients) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (author_key, sent_at) DO NOTHING RETURNING id",
        )
        .bind(story.author.as_deref().unwrap_or(""))
        .bind(&story.author_name)
        .bind(story.sent_at)
        .bind(story.received_at)
        .bind(story.group.as_ref().map(|g| g.0.as_str()))
        .bind(serde_json::to_string(&story.content)?)
        .bind(&story.pointer)
        .bind(story.allows_replies)
        .bind(story.read)
        .bind(expires_at)
        .bind(if story.recipients.is_empty() { None } else { Some(serde_json::to_string(&story.recipients)?) })
        .fetch_optional(&self.writer)
        .await?)
    }

    /// The stories still up, newest first.
    pub async fn stories(&self, now: i64) -> Result<Vec<Story>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM stories WHERE expires_at > ? ORDER BY sent_at DESC"
        )))
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(story_from_row).collect()
    }

    pub async fn story(&self, id: i64) -> Result<Option<Story>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {COLUMNS} FROM stories WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(story_from_row).transpose()
    }

    /// The story `author` (`None`: ours) posted at `sent_at`.
    pub async fn find_story(&self, author: Option<&str>, sent_at: i64) -> Result<Option<Story>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM stories WHERE author_key = ? AND sent_at = ?"
        )))
        .bind(author.unwrap_or(""))
        .bind(sent_at)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(story_from_row).transpose()
    }

    /// Its attachment pointer, to download it.
    pub async fn story_pointer(&self, id: i64) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT pointer FROM stories WHERE id = ?").bind(id).fetch_optional(&self.pool).await?.flatten())
    }

    /// Where its attachment was saved.
    pub async fn set_story_file(&self, id: i64, path: &str) -> Result<()> {
        let mut tx = self.write().await?;
        let content: Option<String> =
            sqlx::query_scalar("SELECT content FROM stories WHERE id = ?").bind(id).fetch_optional(&mut *tx).await?;
        let Some(content) = content else { return Ok(()) };
        let mut content: StoryContent = serde_json::from_str(&content)?;
        if let StoryContent::Media { attachment, .. } = &mut content {
            attachment.local_path = Some(path.to_owned());
        }
        sqlx::query("UPDATE stories SET content = ? WHERE id = ?")
            .bind(serde_json::to_string(&content)?)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks it viewed. Returns whether it wasn't already.
    pub async fn mark_story_read(&self, author: Option<&str>, sent_at: i64) -> Result<bool> {
        let done = sqlx::query("UPDATE stories SET is_read = 1 WHERE author_key = ? AND sent_at = ? AND is_read = 0")
            .bind(author.unwrap_or(""))
            .bind(sent_at)
            .execute(&self.writer)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Someone viewed one of ours (a VIEWED receipt), heard of at `at`.
    /// Returns whether it's one of ours.
    pub async fn add_story_view(&self, sent_at: i64, viewer: &str, at: i64) -> Result<bool> {
        let mut tx = self.write().await?;
        let views: Option<Option<String>> = sqlx::query_scalar("SELECT views FROM stories WHERE author_key = '' AND sent_at = ?")
            .bind(sent_at)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(views) = views else { return Ok(false) };
        let mut views: Vec<StoryView> = views.map(|v| serde_json::from_str(&v)).transpose()?.unwrap_or_default();
        if !views.iter().any(|v| v.viewer == viewer) {
            views.push(StoryView { viewer: viewer.to_owned(), at: Some(at) });
            sqlx::query("UPDATE stories SET views = ? WHERE author_key = '' AND sent_at = ?")
                .bind(serde_json::to_string(&views)?)
                .bind(sent_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// A reply or reaction to a group story. Returns the story's id, or
    /// `None` if it isn't here (Desktop drops those).
    pub async fn add_story_reply(&self, author: Option<&str>, sent_at: i64, reply: &StoryReply) -> Result<Option<i64>> {
        let mut tx = self.write().await?;
        let row: Option<(i64, Option<String>)> =
            sqlx::query_as("SELECT id, replies FROM stories WHERE author_key = ? AND sent_at = ?")
                .bind(author.unwrap_or(""))
                .bind(sent_at)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((id, replies)) = row else { return Ok(None) };
        let mut replies: Vec<StoryReply> = replies.map(|r| serde_json::from_str(&r)).transpose()?.unwrap_or_default();
        if !replies.iter().any(|r| r.author == reply.author && r.sent_at == reply.sent_at) {
            replies.push(reply.clone());
            replies.sort_by_key(|r| r.sent_at);
            sqlx::query("UPDATE stories SET replies = ? WHERE id = ?")
                .bind(serde_json::to_string(&replies)?)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Some(id))
    }

    /// Takes away the stories whose day is up, returning their saved
    /// files to delete.
    pub async fn remove_expired_stories(&self, now: i64) -> Result<Vec<String>> {
        let mut tx = self.write().await?;
        let contents: Vec<String> =
            sqlx::query_scalar("SELECT content FROM stories WHERE expires_at <= ?").bind(now).fetch_all(&mut *tx).await?;
        sqlx::query("DELETE FROM stories WHERE expires_at <= ?").bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(contents
            .iter()
            .filter_map(|c| match serde_json::from_str::<StoryContent>(c).ok()? {
                StoryContent::Media { attachment, .. } => attachment.local_path,
                StoryContent::Text(_) => None,
            })
            .collect())
    }

    /// Who one of ours goes to now (after a story list it went to was
    /// deleted).
    pub async fn set_story_recipients(&self, id: i64, recipients: &[StoryRecipient]) -> Result<()> {
        sqlx::query("UPDATE stories SET recipients = ? WHERE id = ?")
            .bind(serde_json::to_string(recipients)?)
            .bind(id)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Removes one story (deleted for everyone), returning its saved file
    /// to delete.
    pub async fn remove_story(&self, id: i64) -> Result<Option<String>> {
        let content: Option<String> = sqlx::query_scalar("DELETE FROM stories WHERE id = ? RETURNING content")
            .bind(id)
            .fetch_optional(&self.writer)
            .await?;
        Ok(content.and_then(|c| match serde_json::from_str::<StoryContent>(&c).ok()? {
            StoryContent::Media { attachment, .. } => attachment.local_path,
            StoryContent::Text(_) => None,
        }))
    }

    /// Removes every story (stories turned off), returning their saved
    /// files to delete.
    pub async fn remove_all_stories(&self) -> Result<Vec<String>> {
        self.remove_expired_stories(i64::MAX).await
    }

    /// Hides (or shows again) a person's or group's stories: kept in the
    /// `stories_hidden` setting (JSON conversation ids).
    pub async fn set_story_hidden(&self, chat: &ConversationId, hidden: bool) -> Result<()> {
        let mut ids: Vec<String> =
            self.setting(HIDDEN_STORIES).await?.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default();
        ids.retain(|id| id != &chat.0);
        if hidden {
            ids.push(chat.0.clone());
        }
        ids.sort();
        self.set_setting(HIDDEN_STORIES, &serde_json::to_string(&ids)?).await
    }

    /// When the next story goes (unix millis).
    pub async fn next_story_expiry(&self) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT MIN(expires_at) FROM stories").fetch_one(&self.pool).await?)
    }

    /// Whether we share our profile with this person or group (Desktop's
    /// `profileSharing`).
    pub async fn profile_sharing(&self, chat: &ConversationId) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT 1 FROM profile_sharing WHERE id = ?")
            .bind(&chat.0)
            .fetch_optional(&self.pool)
            .await?
            .is_some())
    }

    /// Shares our profile with them (or stops). Returns whether it changed.
    pub async fn set_profile_sharing(&self, chat: &ConversationId, on: bool) -> Result<bool> {
        let done = if on {
            sqlx::query("INSERT OR IGNORE INTO profile_sharing (id) VALUES (?)")
        } else {
            sqlx::query("DELETE FROM profile_sharing WHERE id = ?")
        }
        .bind(&chat.0)
        .execute(&self.writer)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Profile sharing as Storage Service has it (`whitelisted`), for the
    /// chats it has records for; others are left as they are. A
    /// transaction per hundred, so other writes don't wait on thousands.
    pub async fn apply_profile_sharing(&self, chats: &[(ConversationId, bool)]) -> Result<()> {
        for batch in chats.chunks(100) {
            let mut tx = self.write().await?;
            for (chat, on) in batch {
                if *on {
                    sqlx::query("INSERT OR IGNORE INTO profile_sharing (id) VALUES (?)")
                } else {
                    sqlx::query("DELETE FROM profile_sharing WHERE id = ?")
                }
                .bind(&chat.0)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    /// Every group's story send mode that isn't the default.
    pub async fn story_send_modes(&self) -> Result<Vec<(ConversationId, StorySendMode)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT id, mode FROM story_send_modes").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id, mode)| (ConversationId(id), StorySendMode::from_wire(mode))).collect())
    }

    pub async fn story_send_mode(&self, group: &ConversationId) -> Result<StorySendMode> {
        let mode: Option<i64> = sqlx::query_scalar("SELECT mode FROM story_send_modes WHERE id = ?")
            .bind(&group.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(mode.map_or(StorySendMode::IfActive, StorySendMode::from_wire))
    }

    /// Sets groups' story send modes. Returns whether any changed.
    pub async fn set_story_send_modes(&self, modes: &[(ConversationId, StorySendMode)]) -> Result<bool> {
        let mut changed = false;
        for batch in modes.chunks(100) {
            let mut tx = self.write().await?;
            for (group, mode) in batch {
                let done = match mode {
                    StorySendMode::IfActive => {
                        sqlx::query("DELETE FROM story_send_modes WHERE id = ?").bind(&group.0).execute(&mut *tx).await?
                    }
                    mode => sqlx::query(
                        "INSERT INTO story_send_modes (id, mode) VALUES (?1, ?2) \
                         ON CONFLICT (id) DO UPDATE SET mode = ?2 WHERE mode != ?2",
                    )
                    .bind(&group.0)
                    .bind(mode.to_wire())
                    .execute(&mut *tx)
                    .await?,
                };
                changed |= done.rows_affected() > 0;
            }
            tx.commit().await?;
        }
        Ok(changed)
    }
}

/// A group's story send mode (Desktop's `StorySendMode`, Storage Service's
/// `GroupV2Record.StorySendMode`): whether its story is offered when
/// posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorySendMode {
    /// Offered while the group has stories up (`DEFAULT`).
    #[default]
    IfActive,
    /// Removed from the list (`DISABLED`).
    Never,
    /// Always offered (`ENABLED`): we posted to it, or added it.
    Always,
}

impl StorySendMode {
    /// From the protobuf's number (unknown ones are the default).
    pub fn from_wire(value: i64) -> Self {
        match value {
            1 => Self::Never,
            2 => Self::Always,
            _ => Self::IfActive,
        }
    }

    pub fn to_wire(self) -> i64 {
        match self {
            Self::IfActive => 0,
            Self::Never => 1,
            Self::Always => 2,
        }
    }
}
