//! What a message can hold in place of text: a shared contact, a payment
//! notification or a gift badge (Desktop's `contact`, `payment` and
//! `giftBadge` message attributes, `ts/model-types.d.ts`). They're shown,
//! never sent from here: payments and redeeming gift badges happen on the
//! phone.

use serde::{Deserialize, Serialize};

use crate::{AppDb, AttachmentMeta, ConversationId, Result, combine_names, refresh_preview};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageContent {
    /// A shared contact: the first a message lists, as Desktop keeps only
    /// that one (`processContact`).
    Contact(Box<SharedContact>),
    /// A payment event (Desktop's `AnyPaymentEvent`, `ts/types/Payment.std.ts`).
    Payment(PaymentEvent),
    GiftBadge(GiftBadge),
}

/// Desktop's `EmbeddedContactType` (`ts/types/EmbeddedContact.std.ts`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SharedContact {
    #[serde(default)]
    pub given_name: String,
    #[serde(default)]
    pub family_name: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub suffix: String,
    #[serde(default)]
    pub middle_name: String,
    /// The name's own nickname field (`Name.nickname`).
    #[serde(default)]
    pub nickname: String,
    /// Their Signal account, when the sharer's phone knew it (`aciBinary`).
    #[serde(default)]
    pub aci: Option<String>,
    /// The sharer's Signal nickname for them (`SignalNickname`).
    #[serde(default)]
    pub signal_nickname_given: String,
    #[serde(default)]
    pub signal_nickname_family: String,
    /// The sharer's note about them.
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub numbers: Vec<ContactMethod>,
    #[serde(default)]
    pub emails: Vec<ContactMethod>,
    #[serde(default)]
    pub addresses: Vec<PostalAddress>,
    #[serde(default)]
    pub organization: Option<String>,
    /// Their photo: downloaded to `local_path` when it is.
    #[serde(default)]
    pub avatar: Option<AttachmentMeta>,
}

impl SharedContact {
    /// Desktop's `getDisplayName`: given and family name, combined.
    pub fn display_name(&self) -> Option<String> {
        combine_names(&self.given_name, &self.family_name)
    }

    /// Desktop's `getDisplayNickname`: the sharer's nickname for them.
    pub fn display_nickname(&self) -> Option<String> {
        combine_names(&self.signal_nickname_given, &self.signal_nickname_family)
    }
}

/// A phone number or email address and what kind it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactMethod {
    pub value: String,
    pub kind: ContactMethodKind,
    /// A custom kind's name.
    #[serde(default)]
    pub label: Option<String>,
}

/// Desktop's `ContactFormType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactMethodKind {
    Home,
    Mobile,
    Work,
    Custom,
}

impl ContactMethodKind {
    /// The proto's `Phone.Type` / `Email.Type` (1 to 4); anything else is
    /// Home, as Desktop's `parsePhoneItem` and `parseEmailItem` default it.
    pub fn from_wire(value: Option<i32>) -> Self {
        match value {
            Some(2) => Self::Mobile,
            Some(3) => Self::Work,
            Some(4) => Self::Custom,
            _ => Self::Home,
        }
    }
}

/// Desktop's `PostalAddress`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PostalAddress {
    pub kind: AddressKind,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub street: Option<String>,
    #[serde(default)]
    pub pobox: Option<String>,
    #[serde(default)]
    pub neighborhood: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub postcode: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
}

/// Desktop's `AddressType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressKind {
    #[default]
    Home,
    Work,
    Custom,
}

impl AddressKind {
    /// The proto's `PostalAddress.Type` (1 to 3); anything else is Home
    /// (Desktop's `DEFAULT_ADDRESS_TYPE`).
    pub fn from_wire(value: Option<i32>) -> Self {
        match value {
            Some(2) => Self::Work,
            Some(3) => Self::Custom,
            _ => Self::Home,
        }
    }
}

/// Desktop's `PaymentEventKind`: a payment notification (the payment
/// itself is on the phone), or one of the "activate payments" events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PaymentEvent {
    Notification {
        #[serde(default)]
        note: Option<String>,
    },
    ActivationRequest,
    Activated,
}

impl PaymentEvent {
    /// Desktop's `getPaymentEventDescription` (`ts/messages/payments.std.ts`):
    /// `sender` is who sent it (`None`: us), `chat` the chat's title.
    pub fn description(&self, sender: Option<&str>, chat: Option<&str>) -> String {
        match (self, sender) {
            (Self::Notification { .. }, None) => match chat {
                Some(chat) => format!("You started a payment to {chat}"),
                None => "You started a payment".into(),
            },
            (Self::Notification { .. }, Some(sender)) => format!("{sender} started a payment to you"),
            (Self::ActivationRequest, None) => match chat {
                Some(chat) => format!("You sent {chat} a request to activate Payments."),
                None => "You sent a request to activate Payments.".into(),
            },
            (Self::ActivationRequest, Some(sender)) => format!(
                "{sender} wants you to activate Payments. Only send payments to people you trust. Payments can be activated on your mobile device by going to Settings -> Payments."
            ),
            (Self::Activated, None) => "You activated Payments.".into(),
            (Self::Activated, Some(sender)) => format!("{sender} can now accept Payments."),
        }
    }

    /// Desktop shows a notification in a bubble (`renderPayment`), and the
    /// activation events as a timeline notice (`PaymentEventNotification`).
    pub fn is_notification(&self) -> bool {
        matches!(self, Self::Notification { .. })
    }
}

/// Desktop's `giftBadge`: a donation someone made for us (or we for them),
/// redeemed on the phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiftBadge {
    /// The receipt's level: which badge (the subscription configuration's
    /// `levels`).
    pub level: u64,
    /// When the badge runs out, unix millis.
    pub expiration: i64,
    pub state: GiftBadgeState,
    /// The badge for `level`, once looked up.
    #[serde(default)]
    pub badge_id: Option<String>,
}

/// Desktop's `GiftBadgeStates`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GiftBadgeState {
    Unopened,
    /// Ours, viewed on the phone.
    Opened,
    /// Theirs, redeemed on the phone.
    Redeemed,
    Failed,
}

/// What a message with `content` says in the chat list and notifications,
/// with Desktop's emoji (`getNotificationDataForMessage`): `sender` is who
/// sent it (`None`: us), `chat` the chat's title.
pub fn content_preview(content: &MessageContent, sender: Option<&str>, chat: Option<&str>) -> String {
    match content {
        MessageContent::Contact(contact) => {
            format!("👤 {}", contact.display_name().unwrap_or_else(|| "Unknown contact".into()))
        }
        MessageContent::Payment(PaymentEvent::Notification { .. }) => "💳 Payment".into(),
        MessageContent::Payment(event) => format!("💳 {}", event.description(sender, chat)),
        MessageContent::GiftBadge(gift) => match (sender, gift.state) {
            (None, _) => format!("✨ You donated for {}", chat.unwrap_or("Unknown contact")),
            (Some(sender), GiftBadgeState::Unopened) => format!("✨ {sender} donated for you"),
            (Some(_), _) => "✨ You redeemed a donation".into(),
        },
    }
}

impl AppDb {
    /// Sets what a message holds besides its text (a re-delivered message
    /// keeps what it has: a gift badge's state moves on from there).
    pub async fn set_content(&self, message_id: i64, content: &MessageContent) -> Result<()> {
        let mut tx = self.write().await?;
        let conversation: Option<String> = sqlx::query_scalar(
            "UPDATE messages SET content = ? WHERE id = ? AND content IS NULL AND deleted = 0 RETURNING conversation_id",
        )
        .bind(serde_json::to_string(content)?)
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(conversation) = conversation {
            refresh_preview(&mut tx, &ConversationId(conversation)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Changes a message's content with `edit` (`false`: no change).
    async fn edit_content(&self, message_id: i64, edit: impl FnOnce(&mut MessageContent) -> bool) -> Result<bool> {
        let mut tx = self.write().await?;
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT conversation_id, content FROM messages WHERE id = ? AND deleted = 0")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((conversation, Some(json))) = row else { return Ok(false) };
        let mut content: MessageContent = serde_json::from_str(&json)?;
        if !edit(&mut content) {
            return Ok(false);
        }
        sqlx::query("UPDATE messages SET content = ? WHERE id = ?")
            .bind(serde_json::to_string(&content)?)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        refresh_preview(&mut tx, &ConversationId(conversation)).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// A gift badge was viewed on another of our devices: theirs is
    /// redeemed, ours opened (Desktop's `ViewSyncs`, which leaves a failed
    /// one alone). `Ok(false)` when it isn't a gift badge or nothing changed.
    pub async fn gift_badge_viewed(&self, message_id: i64, incoming: bool) -> Result<bool> {
        let to = if incoming { GiftBadgeState::Redeemed } else { GiftBadgeState::Opened };
        self.edit_content(message_id, |content| match content {
            MessageContent::GiftBadge(gift) if gift.state != GiftBadgeState::Failed && gift.state != to => {
                gift.state = to;
                true
            }
            _ => false,
        })
        .await
    }

    /// Names the badge a gift badge's level stands for.
    pub async fn set_gift_badge_id(&self, message_id: i64, badge_id: &str) -> Result<bool> {
        self.edit_content(message_id, |content| match content {
            MessageContent::GiftBadge(gift) if gift.badge_id.as_deref() != Some(badge_id) => {
                gift.badge_id = Some(badge_id.to_owned());
                true
            }
            _ => false,
        })
        .await
    }

    /// Saves where a shared contact's photo was downloaded.
    pub async fn set_contact_avatar_path(&self, message_id: i64, path: &str) -> Result<bool> {
        self.edit_content(message_id, |content| match content {
            MessageContent::Contact(contact) => match &mut contact.avatar {
                Some(avatar) => {
                    avatar.local_path = Some(path.to_owned());
                    true
                }
                None => false,
            },
            _ => false,
        })
        .await
    }
}
