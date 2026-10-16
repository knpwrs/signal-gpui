//! Chat folders, as Signal Desktop keeps them (`ts/types/ChatFolder.std.ts`,
//! `ts/sql/server/chatFolders.std.ts`): the "All chats" folder and custom
//! ones, in order, a deleted one kept (with when) until Storage Service
//! has heard. Kept in one settings row, changed in one transaction.

use serde::{Deserialize, Serialize};

use crate::{AppDb, ConversationId, ConversationKind, Result};

/// Settings key: the chat folders (JSON [`ChatFolder`]s, deleted ones too).
pub const CHAT_FOLDERS: &str = "chat_folders";

/// Desktop's `CHAT_FOLDER_NAME_MAX_CHAR_LENGTH` (graphemes).
pub const CHAT_FOLDER_NAME_MAX: usize = 32;

/// Desktop's `CHAT_FOLDER_DELETED_POSITION` (2^32 - 1).
pub const CHAT_FOLDER_DELETED_POSITION: u32 = u32::MAX;

/// Desktop's `ChatFolderType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatFolderType {
    Unknown,
    All,
    #[default]
    Custom,
}

impl ChatFolderType {
    /// The record's `FolderType`.
    pub fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::All,
            2 => Self::Custom,
            _ => Self::Unknown,
        }
    }

    pub fn to_wire(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::All => 1,
            Self::Custom => 2,
        }
    }
}

/// Desktop's `ChatFolder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFolder {
    /// A UUID.
    pub id: String,
    pub folder_type: ChatFolderType,
    pub name: String,
    pub position: u32,
    pub show_only_unread: bool,
    pub show_muted_chats: bool,
    pub include_all_individual_chats: bool,
    pub include_all_group_chats: bool,
    pub included: Vec<ConversationId>,
    pub excluded: Vec<ConversationId>,
    /// Unix millis; 0 while it's current.
    pub deleted_at: u64,
}

/// What a folder holds (Desktop's `ChatFolderParams`, less its type).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatFolderParams {
    pub name: String,
    pub show_only_unread: bool,
    pub show_muted_chats: bool,
    pub include_all_individual_chats: bool,
    pub include_all_group_chats: bool,
    pub included: Vec<ConversationId>,
    pub excluded: Vec<ConversationId>,
}

impl ChatFolderParams {
    /// Desktop's `CHAT_FOLDER_DEFAULTS`: muted chats shown, nothing else.
    pub fn defaults() -> Self {
        Self { show_muted_chats: true, ..Default::default() }
    }

    /// Desktop's "Unread" suggestion (`CHAT_FOLDER_PRESETS.UNREAD_CHATS`).
    pub fn unread() -> Self {
        Self { show_only_unread: true, include_all_individual_chats: true, include_all_group_chats: true, ..Self::defaults() }
    }

    /// "1:1 chats" (`INDIVIDUAL_CHATS`).
    pub fn individual() -> Self {
        Self { include_all_individual_chats: true, ..Self::defaults() }
    }

    /// "Groups" (`GROUP_CHATS`).
    pub fn groups() -> Self {
        Self { include_all_group_chats: true, ..Self::defaults() }
    }

    /// Desktop's `matchesChatFolderPreset`: the same choices, the name aside.
    pub fn matches(&self, folder: &ChatFolder) -> bool {
        let same = |a: &[ConversationId], b: &[ConversationId]| {
            a.iter().all(|x| b.contains(x)) && b.iter().all(|x| a.contains(x))
        };
        folder.folder_type == ChatFolderType::Custom
            && folder.show_only_unread == self.show_only_unread
            && folder.show_muted_chats == self.show_muted_chats
            && folder.include_all_individual_chats == self.include_all_individual_chats
            && folder.include_all_group_chats == self.include_all_group_chats
            && same(&folder.included, &self.included)
            && same(&folder.excluded, &self.excluded)
    }

    /// Desktop's `validateChatFolderParams`: a name, of at most 32 characters.
    pub fn valid(&self) -> bool {
        use unicode_segmentation::UnicodeSegmentation as _;
        let name = self.name.trim();
        !name.is_empty() && name.graphemes(true).count() <= CHAT_FOLDER_NAME_MAX
    }
}

/// What a chat looks like to a folder (Desktop's
/// `ConversationPropsForChatFolder`).
pub struct FolderChat<'a> {
    pub id: &'a ConversationId,
    pub kind: ConversationKind,
    pub unread: bool,
    /// Muted now: Desktop's `muteExpiresAt > 0`, which is the same thing
    /// there because an ended mute is reset to unmuted
    /// (`ConversationModel.startMuteTimer`). Our `muted_until` keeps its
    /// past time, so this is `Conversation::muted`, not `muted_until > 0`.
    pub muted: bool,
}

impl ChatFolder {
    pub fn is_current(&self) -> bool {
        self.deleted_at == 0
    }

    pub fn params(&self) -> ChatFolderParams {
        ChatFolderParams {
            name: self.name.clone(),
            show_only_unread: self.show_only_unread,
            show_muted_chats: self.show_muted_chats,
            include_all_individual_chats: self.include_all_individual_chats,
            include_all_group_chats: self.include_all_group_chats,
            included: self.included.clone(),
            excluded: self.excluded.clone(),
        }
    }

    fn set_params(&mut self, params: ChatFolderParams) {
        self.name = params.name.trim().to_owned();
        self.show_only_unread = params.show_only_unread;
        self.show_muted_chats = params.show_muted_chats;
        self.include_all_individual_chats = params.include_all_individual_chats;
        self.include_all_group_chats = params.include_all_group_chats;
        self.included = params.included;
        self.excluded = params.excluded;
    }

    /// Desktop's `isConversationInChatFolder`: everything for "All chats";
    /// else included (all 1:1s, all groups, or by name) and not left out
    /// (only unread, muted ones hidden, or excluded by name). Note to Self
    /// counts as a 1:1 chat.
    pub fn contains(&self, chat: &FolderChat) -> bool {
        self.contains_with(chat, false)
    }

    /// [`Self::contains`], with `ignore_filters` leaving out "only unread"
    /// and "include muted" (Desktop's `ignoreShowOnlyUnread` and
    /// `ignoreShowMutedChats`, for the "Add to folder" menu's checks).
    pub fn contains_with(&self, chat: &FolderChat, ignore_filters: bool) -> bool {
        if self.folder_type == ChatFolderType::All {
            return true;
        }
        let direct = matches!(chat.kind, ConversationKind::Direct | ConversationKind::NoteToSelf);
        let included = (self.include_all_individual_chats && direct)
            || (self.include_all_group_chats && chat.kind == ConversationKind::Group)
            || self.included.contains(chat.id);
        let excluded = (!ignore_filters && self.show_only_unread && !chat.unread)
            || (!ignore_filters && !self.show_muted_chats && chat.muted)
            || self.excluded.contains(chat.id);
        included && !excluded
    }
}

/// The current folders, in order (Desktop's `CurrentChatFolders.toSortedArray`).
pub fn current_folders(folders: &[ChatFolder]) -> Vec<ChatFolder> {
    let mut current: Vec<ChatFolder> = folders.iter().filter(|f| f.is_current()).cloned().collect();
    current.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
    current
}

/// A change to the folders.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatFolderEdit {
    /// Desktop's `createChatFolder`: a custom folder with this new id, last.
    Create { id: String, params: ChatFolderParams },
    /// `updateChatFolder`.
    Update { id: String, params: ChatFolderParams },
    /// `deleteChatFolder` at this time (unix millis).
    Delete { id: String, at: u64 },
    /// `updateChatFoldersPositions`: every current folder, "All chats"
    /// included, in this order.
    Reorder(Vec<String>),
    /// `updateChatFolderToggleChat`: a chat added (`true`) or taken out.
    ToggleChat { id: String, chat: ConversationId, add: bool },
    /// `createAllChatsChatFolder`, with this new id: first, the rest moved
    /// along. Nothing when there is one.
    EnsureAll { id: String },
    /// A folder as Storage Service has it (Desktop's `mergeChatFolderRecord`).
    Merge(ChatFolder),
}

/// Desktop's `_resetAllChatFolderPositions`: the current folders numbered
/// from `offset` in their order.
fn renumber(folders: &mut [ChatFolder], offset: u32) {
    let order: Vec<String> = current_folders(folders).into_iter().map(|f| f.id).collect();
    for folder in folders.iter_mut() {
        if let Some(ix) = order.iter().position(|id| *id == folder.id) {
            folder.position = offset + ix as u32;
        }
    }
}

/// Applies `edit`, returning the folders it changed (to save to Storage
/// Service); empty when nothing changed.
pub fn apply_folder_edit(folders: &mut Vec<ChatFolder>, edit: ChatFolderEdit) -> Vec<String> {
    let before = folders.clone();
    match edit {
        ChatFolderEdit::Create { id, params } => {
            let mut folder = ChatFolder {
                id,
                folder_type: ChatFolderType::Custom,
                name: String::new(),
                // After the last (Desktop's `size`, the same when positions
                // run from 0 without gaps; past the last when a merged
                // record left a gap).
                position: current_folders(folders).last().map_or(0, |f| f.position + 1),
                show_only_unread: false,
                show_muted_chats: true,
                include_all_individual_chats: false,
                include_all_group_chats: false,
                included: Vec::new(),
                excluded: Vec::new(),
                deleted_at: 0,
            };
            folder.set_params(params);
            folders.push(folder);
        }
        ChatFolderEdit::Update { id, params } => {
            if let Some(folder) = folders.iter_mut().find(|f| f.id == id && f.is_current()) {
                folder.set_params(params);
            }
        }
        ChatFolderEdit::Delete { id, at } => {
            if let Some(folder) = folders.iter_mut().find(|f| f.id == id && f.is_current()) {
                folder.position = CHAT_FOLDER_DELETED_POSITION;
                folder.deleted_at = at.max(1);
                folder.included.clear();
                folder.excluded.clear();
                renumber(folders, 0);
            }
        }
        // Numbered from 0, as `renumber` and `EnsureAll` number them, so the
        // "All chats" folder stays at 0 and `Create` lands after the last.
        // (Desktop's `updateChatFoldersPositions` numbers from 1; only the
        // order matters to every device.)
        ChatFolderEdit::Reorder(order) => {
            for (ix, id) in order.iter().enumerate() {
                if let Some(folder) = folders.iter_mut().find(|f| f.id == *id && f.is_current()) {
                    folder.position = ix as u32;
                }
            }
        }
        ChatFolderEdit::ToggleChat { id, chat, add } => {
            if let Some(folder) = folders.iter_mut().find(|f| f.id == id && f.is_current()) {
                folder.included.retain(|c| *c != chat);
                folder.excluded.retain(|c| *c != chat);
                if add {
                    folder.included.push(chat);
                } else {
                    folder.excluded.push(chat);
                }
            }
        }
        ChatFolderEdit::EnsureAll { id } => {
            if !folders.iter().any(|f| f.folder_type == ChatFolderType::All && f.is_current()) {
                renumber(folders, 1);
                folders.push(ChatFolder {
                    id,
                    folder_type: ChatFolderType::All,
                    name: String::new(),
                    position: 0,
                    show_only_unread: false,
                    show_muted_chats: true,
                    include_all_individual_chats: true,
                    include_all_group_chats: true,
                    included: Vec::new(),
                    excluded: Vec::new(),
                    deleted_at: 0,
                });
            }
        }
        ChatFolderEdit::Merge(remote) => merge(folders, remote),
    }
    folders
        .iter()
        .filter(|f| before.iter().find(|b| b.id == f.id) != Some(*f))
        .map(|f| f.id.clone())
        .collect()
}

/// Desktop's `mergeChatFolderRecord`: the "All chats" folder takes the
/// remote's id and position; a deletion keeps the earlier time; anything
/// else takes the remote's.
fn merge(folders: &mut Vec<ChatFolder>, mut remote: ChatFolder) {
    let local = folders.iter().position(|f| f.id == remote.id);
    let local_deleted = local.map_or(0, |ix| folders[ix].deleted_at);
    remote.deleted_at = match (remote.deleted_at, local_deleted) {
        (0, local) => local,
        (remote, 0) => remote,
        (remote, local) => remote.min(local),
    };
    if remote.folder_type == ChatFolderType::All {
        match folders.iter_mut().find(|f| f.folder_type == ChatFolderType::All && f.is_current()) {
            Some(all) => {
                all.id = remote.id;
                all.position = remote.position;
            }
            None => folders.push(remote),
        }
        return;
    }
    if remote.deleted_at > 0 {
        remote.position = CHAT_FOLDER_DELETED_POSITION;
        remote.included.clear();
        remote.excluded.clear();
    }
    match local {
        Some(ix) => folders[ix] = remote,
        None => folders.push(remote),
    }
}

impl AppDb {
    /// Every folder, deleted ones too.
    pub async fn chat_folders(&self) -> Result<Vec<ChatFolder>> {
        Ok(self.setting(CHAT_FOLDERS).await?.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
    }

    /// Changes the folders in one transaction; returns those changed.
    pub async fn edit_chat_folders(&self, edit: ChatFolderEdit) -> Result<Vec<String>> {
        let mut tx = self.write().await?;
        let json: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(CHAT_FOLDERS).fetch_optional(&mut *tx).await?;
        let mut folders: Vec<ChatFolder> = json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let changed = apply_folder_edit(&mut folders, edit);
        if changed.is_empty() {
            return Ok(changed);
        }
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
            .bind(CHAT_FOLDERS)
            .bind(serde_json::to_string(&folders)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(changed)
    }
}
