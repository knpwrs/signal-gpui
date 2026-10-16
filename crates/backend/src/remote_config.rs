//! Signal's remote config: the server's per-account feature flags and
//! limits (`GET /v2/config`), as Signal Desktop reads them in
//! `ts/RemoteConfig.dom.ts`.
//!
//! The config is fetched when the app starts (before the message queue,
//! Desktop's `forceRefreshRemoteConfig` on the first connect), and again
//! when the window becomes active if the last fetch was two hours ago or
//! more (Desktop's `maybeRefreshRemoteConfig`). It's kept in the `settings`
//! table so the last values apply before the first fetch, with the
//! server's `ETag` so an unchanged config costs a 304.
//!
//! Scalar keys (`global.*`) override built-in limits; each accessor below
//! falls back to Desktop's default when a key is missing or isn't a number.
//! Version flags (`desktop.<feature>.prod`) name the lowest Desktop version
//! that gets a feature: see [`RemoteConfig::feature`].

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

/// The Signal Desktop release this client's behavior follows, for version
/// flags. Its code is compared with Desktop 8.34 (`v8.34.0-alpha.1`), and
/// it's a production build: only `.prod` flags apply (Desktop's
/// `isProduction`), never `.beta`.
pub const DESKTOP_VERSION: &str = "8.34.0";

/// Settings key: the last config fetched (JSON object of key to value).
pub(crate) const SETTING_CONFIG: &str = "remote_config";
/// Settings key: its `ETag`, sent back as `If-None-Match`.
pub(crate) const SETTING_ETAG: &str = "remote_config.etag";

const MIB: u64 = 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The remote config: every key the server turned on, with its value.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteConfig {
    values: BTreeMap<String, String>,
}

impl RemoteConfig {
    /// The config from a `/v2/config` answer. As Desktop's `getConfig`,
    /// only `desktop.`, `global.` and `cds.` keys are kept; as
    /// `_refreshRemoteConfig`, a key whose value is `false` (any case) is
    /// off, the same as a missing one.
    pub fn from_server(config: impl IntoIterator<Item = (String, String)>) -> Self {
        let values = config
            .into_iter()
            .filter(|(key, _)| ["desktop.", "global.", "cds."].iter().any(|p| key.starts_with(p)))
            .filter(|(_, value)| !value.eq_ignore_ascii_case("false"))
            .collect();
        Self { values }
    }

    /// Builds a config from `(key, value)` pairs (for the demo and tests).
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self::from_server(pairs.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())))
    }

    /// A key's value when it's on (Desktop's `getValue`).
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Whether a key is on (Desktop's `isEnabled`).
    pub fn is_enabled(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// A key as an integer, as JavaScript's `parseInt(value, 10)` reads it
    /// (Desktop's `parseIntOrThrow`): leading digits, after an optional
    /// sign; `None` without any.
    pub fn int(&self, key: &str) -> Option<i64> {
        parse_int(self.value(key)?)
    }

    /// Whether a version-flagged feature is on for us (Desktop's
    /// `_isFeatureEnabledInner` in `ts/util/isFeatureEnabled.dom.ts`):
    /// always for Signal's internal users (`desktop.internalUser`), else when
    /// `desktop.<feature>.prod` is a version no newer than
    /// [`DESKTOP_VERSION`]. A missing or malformed value is off.
    pub fn feature(&self, feature: &str) -> bool {
        if self.is_enabled("desktop.internalUser") {
            return true;
        }
        let ours = semver::Version::parse(DESKTOP_VERSION).expect("DESKTOP_VERSION is a version");
        self.value(&format!("desktop.{feature}.prod"))
            .and_then(|v| semver::Version::parse(v.trim()).ok())
            .is_some_and(|needed| ours >= needed)
    }

    // ------------------------------------------------------------ features

    /// Polls in 1:1 chats (`desktop.pollSend1to1`, Desktop's
    /// `isPollSend1to1Enabled`). Groups always have polls.
    pub fn poll_send_1to1(&self) -> bool {
        self.feature("pollSend1to1")
    }

    /// Acting on group admins' deletes of others' messages
    /// (`isAdminDeleteReceiveEnabled`).
    pub fn admin_delete_receive(&self) -> bool {
        self.feature("adminDelete.receive")
    }

    /// Deleting others' messages for everyone as a group admin
    /// (`isAdminDeleteSendEnabled`).
    pub fn admin_delete_send(&self) -> bool {
        self.feature("adminDelete.send")
    }

    /// Sending a sticker as a reply, with the quote (`stickerReply.send`).
    pub fn sticker_reply_send(&self) -> bool {
        self.feature("stickerReply.send")
    }

    /// Setting our own member label in a group (`groupMemberLabels.edit`).
    pub fn group_member_labels_edit(&self) -> bool {
        self.feature("groupMemberLabels.edit")
    }

    /// Admins ending a group for everyone (`groupTerminate.send`).
    pub fn group_terminate_send(&self) -> bool {
        self.feature("groupTerminate.send")
    }

    /// Service ids in the binary proto fields instead of strings
    /// (`binaryServiceId`, Desktop's `isProtoBinaryEncodingEnabled`).
    pub fn binary_service_id(&self) -> bool {
        self.feature("binaryServiceId")
    }

    /// How a message we send names `sid` (a service id string), as Desktop
    /// fills each pair of proto fields (`ts/textsecure/SendMessage.preload.ts`):
    /// with `binaryServiceId` on, only the `*Binary` field; otherwise only
    /// the string. Something that isn't a service id goes as the string.
    pub fn wire_service_id(&self, sid: &str) -> WireServiceId {
        use presage::libsignal_service::protocol::ServiceId;
        match ServiceId::parse_from_service_id_string(sid).filter(|_| self.binary_service_id()) {
            Some(id) => WireServiceId::Binary(id.service_id_binary()),
            None => WireServiceId::String(sid.to_owned()),
        }
    }

    // -------------------------------------------------------------- limits

    /// How long after sending you can delete for everyone, in millis
    /// (`global.normalDeleteMaxAgeInSeconds`; Desktop's
    /// `getNormalDeleteMaxAgeMs`: a positive number of seconds, else a day).
    pub fn normal_delete_max_age_ms(&self) -> i64 {
        self.max_age_ms("global.normalDeleteMaxAgeInSeconds")
    }

    /// How long after a message was sent a group admin can delete it
    /// (`global.adminDeleteMaxAgeInSeconds`, `getAdminDeleteMaxAgeMs`).
    pub fn admin_delete_max_age_ms(&self) -> i64 {
        self.max_age_ms("global.adminDeleteMaxAgeInSeconds")
    }

    /// How long the server keeps undelivered messages, in millis
    /// (`global.messageQueueTimeInSeconds`; Desktop's `getMessageQueueTime`:
    /// never under 30 days). View-once messages left unviewed go after it.
    pub fn message_queue_time_ms(&self) -> i64 {
        let floor = signal_store::VIEW_ONCE_MIN_AGE_MS / 1000;
        self.int("global.messageQueueTimeInSeconds").unwrap_or(floor).max(floor).saturating_mul(1000)
    }

    fn max_age_ms(&self, key: &str) -> i64 {
        // Desktop's `safeParseInteger`: the whole value must be a number.
        self.value(key)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(DAY_MS, |secs| secs.saturating_mul(1000))
    }

    /// Most chats you can pin (`global.pinnedChatLimit`, Desktop's
    /// `getPinnedConversationLimit`: 4).
    pub fn pinned_chat_limit(&self) -> usize {
        self.count("global.pinnedChatLimit", 4)
    }

    /// Most messages pinned in a chat (`global.pinnedMessageLimit`,
    /// `getPinnedMessagesLimit`: 3).
    pub fn pinned_message_limit(&self) -> usize {
        self.count("global.pinnedMessageLimit", 3)
    }

    /// The biggest attachment you can send, in bytes
    /// (`global.attachments.maxBytes`; Desktop's
    /// `getMaximumOutgoingAttachmentSize` in `ts/types/AttachmentSize.std.ts`:
    /// 100 MiB).
    pub fn attachment_max_bytes(&self) -> u64 {
        self.bytes("global.attachments.maxBytes", 100 * MIB)
    }

    /// The biggest attachment downloaded (`global.attachments.maxReceiveBytes`;
    /// `getMaximumIncomingAttachmentSize`: 1.25 times the send limit).
    pub fn attachment_max_receive_bytes(&self) -> u64 {
        self.int("global.attachments.maxReceiveBytes")
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or_else(|| self.attachment_max_bytes() * 5 / 4)
    }

    /// Attachments smaller than this download on their own
    /// (`global.attachments.maxAutoDownloadSizeBytes`;
    /// `getMaximumAutoDownloadSize`: 200 MiB).
    pub fn auto_download_max_bytes(&self) -> u64 {
        self.bytes("global.attachments.maxAutoDownloadSizeBytes", 200 * MIB)
    }

    /// Whether an attachment of `size` bytes downloads without asking
    /// (Desktop's `isAutoDownloadEnabled`: smaller than the limit).
    pub fn auto_downloads(&self, size: u64) -> bool {
        size < self.auto_download_max_bytes()
    }

    /// Members a group should have at most (`global.groupsv2.maxGroupSize`;
    /// Desktop's `getGroupSizeRecommendedLimit(151)`), counting us.
    pub fn group_size_recommended(&self) -> usize {
        self.count("global.groupsv2.maxGroupSize", 151)
    }

    /// Members a group can have at most, counting us
    /// (`global.groupsv2.groupSizeHardLimit`): as Desktop's
    /// `toggleConversationInChooseMembers`, `getGroupSizeHardLimit(1001)`
    /// and never below the recommended limit plus one.
    pub fn group_size_hard_limit(&self) -> usize {
        self.count("global.groupsv2.groupSizeHardLimit", 1001).max(self.group_size_recommended() + 1)
    }

    /// Shortest username nickname, the part of a username before the dot and
    /// digits; not a contact's nickname (`global.nicknames.min`, Desktop's
    /// `getMinNickname` in `ts/util/Username.dom.ts`: 3).
    pub fn username_nickname_min(&self) -> usize {
        self.count("global.nicknames.min", 3)
    }

    /// Longest username nickname (`global.nicknames.max`, `getMaxNickname`: 32).
    pub fn username_nickname_max(&self) -> usize {
        self.count("global.nicknames.max", 32)
    }

    fn count(&self, key: &str, fallback: usize) -> usize {
        self.int(key).and_then(|n| usize::try_from(n).ok()).unwrap_or(fallback)
    }

    fn bytes(&self, key: &str, fallback: u64) -> u64 {
        self.int(key).and_then(|n| u64::try_from(n).ok()).unwrap_or(fallback)
    }
}

/// A service id in one of a proto's two fields for it
/// ([`RemoteConfig::wire_service_id`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireServiceId {
    /// The string field (`authorAci`, `senderAci`, ...).
    String(String),
    /// The binary field (`authorAciBinary`, ...): 16 bytes for an ACI.
    Binary(Vec<u8>),
}

impl WireServiceId {
    /// The string field's value: `None` when the binary field is used.
    pub fn string(&self) -> Option<String> {
        match self {
            Self::String(s) => Some(s.clone()),
            Self::Binary(_) => None,
        }
    }

    /// The binary field's value: `None` when the string field is used.
    pub fn binary(&self) -> Option<Vec<u8>> {
        match self {
            Self::Binary(b) => Some(b.clone()),
            Self::String(_) => None,
        }
    }
}

/// JavaScript's `parseInt(value, 10)`, for whole numbers.
fn parse_int(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    let number: i64 = digits[..end].parse().ok()?;
    Some(if negative { -number } else { number })
}

static CURRENT: LazyLock<RwLock<Arc<RemoteConfig>>> = LazyLock::new(Default::default);

/// The config in effect: the last one fetched (or loaded at start), or an
/// empty one, where every limit is Desktop's default and every version
/// flag is off, as Desktop is before its first fetch.
pub fn current() -> Arc<RemoteConfig> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Puts `config` in effect; returns whether it differs from the last one.
pub(crate) fn set_current(config: RemoteConfig) -> bool {
    let mut current = CURRENT.write().unwrap_or_else(|e| e.into_inner());
    if **current == config {
        return false;
    }
    *current = Arc::new(config);
    true
}

/// The config saved in the database, if any.
pub(crate) async fn load(db: &signal_store::AppDb) -> Option<RemoteConfig> {
    let json = db.setting(SETTING_CONFIG).await.ok().flatten()?;
    serde_json::from_str(&json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_desktops_keys_and_drops_false() {
        let config = RemoteConfig::from_server([
            ("global.pinnedChatLimit".to_owned(), "6".to_owned()),
            ("desktop.pollSend1to1.prod".to_owned(), "8.0.0".to_owned()),
            ("android.something".to_owned(), "true".to_owned()),
            ("desktop.internalUser".to_owned(), "FALSE".to_owned()),
            ("cds.limit".to_owned(), "1".to_owned()),
        ]);
        assert_eq!(config.value("global.pinnedChatLimit"), Some("6"));
        assert!(config.is_enabled("cds.limit"));
        assert!(!config.is_enabled("android.something"));
        assert!(!config.is_enabled("desktop.internalUser"));
    }

    #[test]
    fn version_flags_compare_with_our_desktop_version() {
        let flag = |v: &str| RemoteConfig::from_pairs([("desktop.pollSend1to1.prod", v)]);
        assert!(flag("8.0.0").poll_send_1to1());
        assert!(flag(DESKTOP_VERSION).poll_send_1to1());
        assert!(!flag("8.34.1").poll_send_1to1());
        assert!(!flag("99.0.0").poll_send_1to1());
        // Not a version, or only the beta key: off, as Desktop.
        assert!(!flag("true").poll_send_1to1());
        assert!(!RemoteConfig::from_pairs([("desktop.pollSend1to1.beta", "1.0.0")]).poll_send_1to1());
        assert!(!RemoteConfig::default().poll_send_1to1());
        // Internal users get everything.
        assert!(RemoteConfig::from_pairs([("desktop.internalUser", "true")]).group_terminate_send());
    }

    #[test]
    fn limits_fall_back_to_desktops_defaults() {
        let empty = RemoteConfig::default();
        assert_eq!(empty.normal_delete_max_age_ms(), DAY_MS);
        assert_eq!(empty.admin_delete_max_age_ms(), DAY_MS);
        assert_eq!(empty.pinned_chat_limit(), 4);
        assert_eq!(empty.pinned_message_limit(), 3);
        assert_eq!(empty.attachment_max_bytes(), 100 * MIB);
        assert_eq!(empty.attachment_max_receive_bytes(), 125 * MIB);
        assert_eq!(empty.auto_download_max_bytes(), 200 * MIB);
        assert_eq!(empty.group_size_recommended(), 151);
        assert_eq!(empty.group_size_hard_limit(), 1001);
        assert_eq!((empty.username_nickname_min(), empty.username_nickname_max()), (3, 32));
        assert!(empty.auto_downloads(200 * MIB - 1));
        assert!(!empty.auto_downloads(200 * MIB));
    }

    #[test]
    fn the_message_queue_time_is_never_under_30_days() {
        let month = 30 * DAY_MS;
        let queue = |v: &str| RemoteConfig::from_pairs([("global.messageQueueTimeInSeconds", v)]).message_queue_time_ms();
        assert_eq!(RemoteConfig::default().message_queue_time_ms(), month);
        assert_eq!(queue("5184000"), 2 * month);
        assert_eq!(queue("60"), month);
        assert_eq!(queue("soon"), month);
    }

    #[test]
    fn limits_follow_the_server() {
        let config = RemoteConfig::from_pairs([
            ("global.normalDeleteMaxAgeInSeconds", "3600"),
            ("global.adminDeleteMaxAgeInSeconds", "0"),
            ("global.pinnedChatLimit", "5"),
            ("global.pinnedMessageLimit", "10 pins"),
            ("global.attachments.maxBytes", "1000"),
            ("global.attachments.maxAutoDownloadSizeBytes", "nope"),
            ("global.groupsv2.maxGroupSize", "2000"),
            ("global.groupsv2.groupSizeHardLimit", "1500"),
        ]);
        assert_eq!(config.normal_delete_max_age_ms(), 3_600_000);
        // Not positive: the default.
        assert_eq!(config.admin_delete_max_age_ms(), DAY_MS);
        assert_eq!(config.pinned_chat_limit(), 5);
        // `parseInt` reads leading digits.
        assert_eq!(config.pinned_message_limit(), 10);
        assert_eq!(config.attachment_max_bytes(), 1000);
        assert_eq!(config.attachment_max_receive_bytes(), 1250);
        assert_eq!(config.auto_download_max_bytes(), 200 * MIB);
        // The hard limit is always above the recommended one.
        assert_eq!(config.group_size_recommended(), 2000);
        assert_eq!(config.group_size_hard_limit(), 2001);
        let receive = RemoteConfig::from_pairs([("global.attachments.maxReceiveBytes", "77")]);
        assert_eq!(receive.attachment_max_receive_bytes(), 77);
    }

    #[test]
    fn service_ids_go_in_one_field_by_the_flag() {
        let aci = "6b1a2b3c-0000-4000-8000-000000000001";
        let pni = "PNI:6b1a2b3c-0000-4000-8000-000000000002";
        let off = RemoteConfig::default();
        assert_eq!(off.wire_service_id(aci), WireServiceId::String(aci.into()));
        let on = RemoteConfig::from_pairs([("desktop.binaryServiceId.prod", "8.0.0")]);
        let binary = on.wire_service_id(aci);
        assert_eq!(binary.binary().map(|b| b.len()), Some(16), "an ACI is its bare UUID");
        assert_eq!(binary.string(), None);
        assert_eq!(on.wire_service_id(pni).binary().map(|b| (b.len(), b[0])), Some((17, 1)), "a PNI has a kind byte");
        assert_eq!(on.wire_service_id("not a service id"), WireServiceId::String("not a service id".into()));
    }

    #[test]
    fn parses_ints_like_javascript() {
        assert_eq!(parse_int(" 42"), Some(42));
        assert_eq!(parse_int("-7x"), Some(-7));
        assert_eq!(parse_int("1.5"), Some(1));
        assert_eq!(parse_int("x1"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn size_limits_read_like_desktop() {
        assert_eq!(crate::size_limit_label(100 * MIB), "100 MB");
        assert_eq!(crate::size_limit_label(125 * MIB), "125 MB");
        assert_eq!(crate::size_limit_label(1536), "1 kB");
        assert_eq!(crate::size_limit_label(3 * 1024 * MIB), "3 GB");
    }

    #[test]
    fn round_trips_through_json() {
        let config = RemoteConfig::from_pairs([("global.pinnedChatLimit", "6")]);
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<RemoteConfig>(&json).unwrap(), config);
    }
}
