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
    /// Ours: who viewed it.
    pub views: Vec<String>,
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
}

const COLUMNS: &str = "id, author_key, author_name, sent_at, received_at, group_id, content, allows_replies, is_read, \
                       expires_at, replies, views";

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
                allows_replies, is_read, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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

    /// Someone viewed one of ours (a VIEWED receipt). Returns whether it's
    /// one of ours.
    pub async fn add_story_view(&self, sent_at: i64, viewer: &str) -> Result<bool> {
        let mut tx = self.write().await?;
        let views: Option<Option<String>> = sqlx::query_scalar("SELECT views FROM stories WHERE author_key = '' AND sent_at = ?")
            .bind(sent_at)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(views) = views else { return Ok(false) };
        let mut views: Vec<String> = views.map(|v| serde_json::from_str(&v)).transpose()?.unwrap_or_default();
        if !views.iter().any(|v| v == viewer) {
            views.push(viewer.to_owned());
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
}
