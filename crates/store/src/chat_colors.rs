//! Chat colors: the color of our message bubbles, as Signal Desktop keeps
//! them (`ts/types/Colors.std.ts`, `ts/state/ducks/items.preload.ts`): a
//! default for every chat, a choice per chat over it, and custom colors
//! (a solid hue or a gradient of two). Desktop keeps them on the device;
//! they come from elsewhere only in the history a link transfers
//! (`ChatStyle` in the backup). Kept here in three settings rows.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{AppDb, ConversationId, Result};

/// Settings key: the default chat color (JSON [`ChatColor`]).
pub const CHAT_COLOR_DEFAULT: &str = "chat_colors.default";
/// Settings key: the custom colors, in the order made (JSON [`CustomColors`]).
pub const CHAT_COLORS_CUSTOM: &str = "chat_colors.custom";
/// Settings key: each chat's own color (JSON map of conversation id to
/// [`ChatColor`]).
pub const CHAT_COLORS_CHATS: &str = "chat_colors.chats";

/// Desktop's `ConversationColors`, in its picker's order.
pub const CHAT_COLOR_PRESETS: [&str; 22] = [
    "ultramarine",
    "crimson",
    "vermilion",
    "burlap",
    "forest",
    "wintergreen",
    "teal",
    "blue",
    "indigo",
    "violet",
    "plum",
    "taupe",
    "steel",
    "ember",
    "midnight",
    "infrared",
    "lagoon",
    "fluorescent",
    "basil",
    "sublime",
    "sea",
    "tangerine",
];

/// One end of a custom color (Desktop's `{ hue, saturation, lightness? }`):
/// hue in degrees, saturation in percent, lightness 0 to 1 when given (a
/// color from a backup); without it, the hue sets it (`getHSL`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HueSaturation {
    pub hue: f32,
    pub saturation: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightness: Option<f32>,
}

/// Desktop's `CustomColorType`: solid, or a gradient at `deg` (Desktop's
/// own angle, turned into CSS's by `270 - deg`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CustomColor {
    pub start: HueSaturation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<HueSaturation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deg: Option<f32>,
}

impl CustomColor {
    /// Desktop's `ULTRAMARINE_ISH`, where a new custom color starts.
    pub const ULTRAMARINE_ISH: CustomColor =
        CustomColor { start: HueSaturation { hue: 220., saturation: 84., lightness: None }, end: None, deg: Some(180.) };
}

/// A chat color: one of Desktop's presets by name, or a custom color (its
/// id and a copy of it, as Desktop's conversations keep `customColor`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatColor {
    Preset { name: String },
    Custom { id: String, value: CustomColor },
}

impl Default for ChatColor {
    /// Desktop's `DEFAULT_CONVERSATION_COLOR`.
    fn default() -> Self {
        Self::Preset { name: CHAT_COLOR_PRESETS[0].into() }
    }
}

impl ChatColor {
    pub fn preset(name: &str) -> Self {
        Self::Preset { name: name.into() }
    }

    pub fn custom_id(&self) -> Option<&str> {
        match self {
            Self::Custom { id, .. } => Some(id),
            Self::Preset { .. } => None,
        }
    }
}

/// Desktop's `customColors` item: each by id, and the order they were made.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CustomColors {
    pub colors: Vec<(String, CustomColor)>,
}

impl CustomColors {
    pub fn get(&self, id: &str) -> Option<CustomColor> {
        self.colors.iter().find(|(i, _)| i == id).map(|(_, c)| *c)
    }
}

/// Every chat color setting, as the app reads them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatColors {
    pub default: ChatColor,
    pub custom: CustomColors,
    pub chats: BTreeMap<String, ChatColor>,
}

impl ChatColors {
    /// The color `chat` shows (Desktop's `getConversationColorAttributes`):
    /// its own, else the default.
    pub fn of(&self, chat: &ConversationId) -> &ChatColor {
        self.chats.get(&chat.0).unwrap_or(&self.default)
    }

    /// The chats that use custom color `id` (Desktop's
    /// `getConversationsWithCustomColor`).
    pub fn chats_with(&self, id: &str) -> usize {
        self.chats.values().filter(|c| c.custom_id() == Some(id)).count()
    }
}

/// What to change.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatColorEdit {
    /// A chat's own color, or (`None`) back to the default (Desktop's
    /// `colorSelected`).
    Chat(ConversationId, Option<ChatColor>),
    /// The default (`setGlobalDefaultConversationColor`).
    Default(ChatColor),
    /// Desktop's `resetDefaultChatColor`: the default goes back to ultramarine.
    ResetDefault,
    /// `resetAllChatColors`: every chat back to the default.
    ResetChats,
    /// `addCustomColor` with this new id, chosen for the chat given, else
    /// as the default.
    AddCustom { id: String, color: CustomColor, chat: Option<ConversationId> },
    /// `editCustomColor`: the color with that id changes.
    EditCustom { id: String, color: CustomColor },
    /// `removeCustomColor` and `removeCustomColorOnConversations`: gone,
    /// from every chat using it too, and the default reset, as Desktop
    /// does whichever color it was.
    RemoveCustom { id: String },
    /// Everything at once, from a link's history transfer.
    Import(ChatColors),
}

impl ChatColors {
    /// Applies `edit`; `false` when nothing changed.
    pub fn apply(&mut self, edit: ChatColorEdit) -> bool {
        let before = self.clone();
        match edit {
            ChatColorEdit::Chat(chat, Some(color)) => {
                self.chats.insert(chat.0, color);
            }
            ChatColorEdit::Chat(chat, None) => {
                self.chats.remove(&chat.0);
            }
            ChatColorEdit::Default(color) => self.default = color,
            ChatColorEdit::ResetDefault => self.default = ChatColor::default(),
            ChatColorEdit::ResetChats => self.chats.clear(),
            ChatColorEdit::AddCustom { id, color, chat } => {
                self.custom.colors.retain(|(i, _)| *i != id);
                self.custom.colors.push((id.clone(), color));
                let chosen = ChatColor::Custom { id, value: color };
                match chat {
                    Some(chat) => {
                        self.chats.insert(chat.0, chosen);
                    }
                    None => self.default = chosen,
                }
            }
            ChatColorEdit::EditCustom { id, color } => {
                if let Some((_, existing)) = self.custom.colors.iter_mut().find(|(i, _)| *i == id) {
                    *existing = color;
                }
            }
            ChatColorEdit::RemoveCustom { id } => {
                self.custom.colors.retain(|(i, _)| *i != id);
                self.chats.retain(|_, c| c.custom_id() != Some(id.as_str()));
                self.default = ChatColor::default();
            }
            ChatColorEdit::Import(colors) => *self = colors,
        }
        *self != before
    }
}

/// A JSON setting, or its default when unset or unreadable.
fn read_json<T: serde::de::DeserializeOwned + Default>(json: Option<String>) -> T {
    json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
}

impl AppDb {
    /// Every chat color setting.
    pub async fn chat_colors(&self) -> Result<ChatColors> {
        Ok(ChatColors {
            default: read_json(self.setting(CHAT_COLOR_DEFAULT).await?),
            custom: read_json(self.setting(CHAT_COLORS_CUSTOM).await?),
            chats: read_json(self.setting(CHAT_COLORS_CHATS).await?),
        })
    }

    /// Changes the chat colors in one transaction, so two changes at once
    /// can't undo each other. `Ok(false)` when nothing changed.
    pub async fn edit_chat_colors(&self, edit: ChatColorEdit) -> Result<bool> {
        let mut tx = self.write().await?;
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM settings WHERE key IN (?, ?, ?)")
            .bind(CHAT_COLOR_DEFAULT)
            .bind(CHAT_COLORS_CUSTOM)
            .bind(CHAT_COLORS_CHATS)
            .fetch_all(&mut *tx)
            .await?;
        let value = |key: &str| rows.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        let mut colors = ChatColors {
            default: value(CHAT_COLOR_DEFAULT).and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default(),
            custom: value(CHAT_COLORS_CUSTOM).and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default(),
            chats: value(CHAT_COLORS_CHATS).and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default(),
        };
        if !colors.apply(edit) {
            return Ok(false);
        }
        for (key, json) in [
            (CHAT_COLOR_DEFAULT, serde_json::to_string(&colors.default)?),
            (CHAT_COLORS_CUSTOM, serde_json::to_string(&colors.custom)?),
            (CHAT_COLORS_CHATS, serde_json::to_string(&colors.chats)?),
        ] {
            sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)").bind(key).bind(json).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}
