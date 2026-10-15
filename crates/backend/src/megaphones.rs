//! Megaphones: Signal's banners at the bottom of the chat list, from the
//! same manifest as the release notes, kept and shown as Desktop does
//! (`ts/services/megaphone.preload.ts`, `ts/types/Megaphone.std.ts`, the
//! `megaphones` duck): each has dates to show between, an optional
//! condition, and up to two buttons (donate, snooze, finish).

use serde::{Deserialize, Serialize};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Desktop's snooze when the megaphone doesn't say (`SNOOZE_DEFAULT_DURATION_DAYS`).
const SNOOZE_DEFAULT_DAYS: i64 = 3;
/// Desktop's `CONDITIONAL_STANDARD_DONATE_DEVICE_AGE`.
const STANDARD_DONATE_DEVICE_AGE_MS: i64 = 7 * DAY_MS;

/// A megaphone as the manifest lists it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestMegaphone {
    pub uuid: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub countries: Option<String>,
    #[serde(default)]
    pub desktop_min_version: Option<String>,
    pub dont_show_before_epoch_seconds: i64,
    pub dont_show_after_epoch_seconds: i64,
    #[serde(default)]
    pub show_for_number_of_days: i64,
    #[serde(default)]
    pub conditional_id: Option<String>,
    #[serde(default)]
    pub primary_cta_id: Option<String>,
    #[serde(default)]
    pub primary_cta_data: Option<serde_json::Value>,
    #[serde(default)]
    pub secondary_cta_id: Option<String>,
    #[serde(default)]
    pub secondary_cta_data: Option<serde_json::Value>,
}

/// Its words in one language (Desktop's `megaphoneSchema`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MegaphoneText {
    pub uuid: String,
    #[serde(default)]
    pub image: Option<String>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub primary_cta_text: Option<String>,
    #[serde(default)]
    pub secondary_cta_text: Option<String>,
}

/// A saved megaphone and what's been done with it (Desktop's
/// `RemoteMegaphoneType`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Megaphone {
    pub id: String,
    pub priority: i64,
    pub dont_show_before_ms: i64,
    pub dont_show_after_ms: i64,
    pub conditional_id: Option<String>,
    pub primary_cta_id: Option<String>,
    pub primary_cta_data: Option<serde_json::Value>,
    pub secondary_cta_id: Option<String>,
    pub secondary_cta_data: Option<serde_json::Value>,
    pub title: String,
    pub body: String,
    pub primary_cta_text: Option<String>,
    pub secondary_cta_text: Option<String>,
    /// The downloaded image.
    pub image_path: String,
    pub locale: String,
    pub snoozed_at: Option<i64>,
    pub snooze_count: u32,
    pub finished: bool,
}

/// What the chat list shows of one.
#[derive(Debug, Clone, PartialEq)]
pub struct MegaphoneView {
    pub id: String,
    pub title: String,
    pub body: String,
    pub image_path: String,
    pub primary: Option<(MegaphoneCta, String)>,
    pub secondary: Option<(MegaphoneCta, String)>,
}

/// Desktop's `MegaphoneCtaId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MegaphoneCta {
    Donate,
    Finish,
    Snooze,
}

impl MegaphoneCta {
    fn parse(id: &str) -> Option<Self> {
        match id {
            "donate" => Some(Self::Donate),
            "finish" => Some(Self::Finish),
            "snooze" => Some(Self::Snooze),
            _ => None,
        }
    }
}

/// What a megaphone's condition depends on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Conditions {
    /// When this device was linked (unix millis).
    pub device_created_at: Option<i64>,
    /// Whether our profile has donor badges (`None`: couldn't tell).
    pub has_badges: Option<bool>,
}

impl Megaphone {
    pub fn new(from: &ManifestMegaphone, text: MegaphoneText, locale: String, image_path: String) -> Self {
        Self {
            id: from.uuid.clone(),
            priority: from.priority,
            dont_show_before_ms: from.dont_show_before_epoch_seconds.saturating_mul(1000),
            dont_show_after_ms: from.dont_show_after_epoch_seconds.saturating_mul(1000),
            conditional_id: from.conditional_id.clone(),
            primary_cta_id: from.primary_cta_id.clone(),
            primary_cta_data: from.primary_cta_data.clone(),
            secondary_cta_id: from.secondary_cta_id.clone(),
            secondary_cta_data: from.secondary_cta_data.clone(),
            title: text.title,
            body: text.body,
            primary_cta_text: text.primary_cta_text,
            secondary_cta_text: text.secondary_cta_text,
            image_path,
            locale,
            snoozed_at: None,
            snooze_count: 0,
            finished: false,
        }
    }

    /// Past its last day: Desktop deletes it.
    pub fn expired(&self, now: i64) -> bool {
        now > self.dont_show_after_ms
    }

    /// Whether the condition, if any, holds (Desktop's `isConditionalActive`;
    /// `internal_user` and `test` are for Signal's own builds).
    pub fn condition_met(&self, conditions: &Conditions, now: i64) -> bool {
        match self.conditional_id.as_deref() {
            None => true,
            Some("standard_donate") => {
                conditions.device_created_at.is_some_and(|at| now - at >= STANDARD_DONATE_DEVICE_AGE_MS)
                    && conditions.has_badges == Some(false)
            }
            Some(_) => false,
        }
    }

    /// Desktop's `isMegaphoneShowable`.
    pub fn showable(&self, conditions: &Conditions, now: i64) -> bool {
        if self.finished || now < self.dont_show_before_ms || now > self.dont_show_after_ms {
            return false;
        }
        let known = |id: &Option<String>| id.as_deref().is_none_or(|id| MegaphoneCta::parse(id).is_some());
        if !known(&self.primary_cta_id) || !known(&self.secondary_cta_id) {
            return false;
        }
        if !self.condition_met(conditions, now) {
            return false;
        }
        match self.snoozed_at {
            Some(at) => now >= at + self.snooze_ms(),
            None => true,
        }
    }

    /// How long the last snooze lasts (Desktop's
    /// `getMegaphoneLastSnoozeDurationMs`): the snooze button's
    /// `snoozeDurationDays`, one per snooze, the last repeating.
    fn snooze_ms(&self) -> i64 {
        let data = if self.primary_cta_id.as_deref() == Some("snooze") {
            self.primary_cta_data.as_ref()
        } else if self.secondary_cta_id.as_deref() == Some("snooze") {
            self.secondary_cta_data.as_ref()
        } else {
            None
        };
        let days: Vec<i64> = data
            .and_then(|d| d.get("snoozeDurationDays"))
            .and_then(|d| d.as_array())
            .map(|d| d.iter().filter_map(serde_json::Value::as_i64).collect())
            .filter(|d: &Vec<i64>| !d.is_empty())
            .unwrap_or_else(|| vec![SNOOZE_DEFAULT_DAYS]);
        let ix = (self.snooze_count.saturating_sub(1) as usize).min(days.len() - 1);
        days[ix] * DAY_MS
    }

    /// A button pressed (Desktop's `interactWithMegaphone`): donate and
    /// finish are done with it, snooze puts it off.
    pub fn press(&mut self, cta: MegaphoneCta, now: i64) {
        match cta {
            MegaphoneCta::Donate | MegaphoneCta::Finish => self.finished = true,
            MegaphoneCta::Snooze => {
                self.snoozed_at = Some(now);
                self.snooze_count += 1;
            }
        }
    }

    pub fn view(&self) -> MegaphoneView {
        let button = |id: &Option<String>, text: &Option<String>| {
            let cta = MegaphoneCta::parse(id.as_deref()?)?;
            Some((cta, text.clone().unwrap_or_default()))
        };
        MegaphoneView {
            id: self.id.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            image_path: self.image_path.clone(),
            primary: button(&self.primary_cta_id, &self.primary_cta_text),
            secondary: button(&self.secondary_cta_id, &self.secondary_cta_text),
        }
    }
}

/// The ones to show, highest priority first.
pub fn visible(megaphones: &[Megaphone], conditions: &Conditions, now: i64) -> Vec<MegaphoneView> {
    let mut shown: Vec<&Megaphone> = megaphones.iter().filter(|m| m.showable(conditions, now)).collect();
    shown.sort_by_key(|m| std::cmp::Reverse(m.priority));
    shown.into_iter().map(Megaphone::view).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn megaphone() -> Megaphone {
        let manifest: ManifestMegaphone = serde_json::from_value(serde_json::json!({
            "uuid": "m1",
            "priority": 100,
            "desktopMinVersion": "8.0.0",
            "dontShowBeforeEpochSeconds": NOW / 1000 - 10,
            "dontShowAfterEpochSeconds": NOW / 1000 + 30 * 86_400,
            "showForNumberOfDays": 30,
            "primaryCtaId": "donate",
            "secondaryCtaId": "snooze",
            "secondaryCtaData": { "snoozeDurationDays": [5, 7] }
        }))
        .unwrap();
        let text: MegaphoneText = serde_json::from_value(serde_json::json!({
            "uuid": "m1", "image": "/static/x.png", "title": "Title", "body": "Body",
            "primaryCtaText": "Donate", "secondaryCtaText": "Not now"
        }))
        .unwrap();
        Megaphone::new(&manifest, text, "en".into(), "/tmp/x.png".into())
    }

    #[test]
    fn shown_between_its_dates_until_done() {
        let none = Conditions::default();
        let mut m = megaphone();
        assert!(m.showable(&none, NOW));
        assert!(!m.showable(&none, NOW - 60_000), "not yet");
        assert!(!m.showable(&none, m.dont_show_after_ms + 1) && m.expired(m.dont_show_after_ms + 1));
        let view = m.view();
        assert_eq!(view.primary, Some((MegaphoneCta::Donate, "Donate".into())));
        assert_eq!(view.secondary, Some((MegaphoneCta::Snooze, "Not now".into())));
        m.press(MegaphoneCta::Donate, NOW);
        assert!(!m.showable(&none, NOW), "donated");
    }

    #[test]
    fn snoozes_follow_the_buttons_days() {
        let none = Conditions::default();
        let mut m = megaphone();
        m.press(MegaphoneCta::Snooze, NOW);
        assert!(!m.showable(&none, NOW + 4 * DAY_MS));
        assert!(m.showable(&none, NOW + 5 * DAY_MS), "first snooze: 5 days");
        m.press(MegaphoneCta::Snooze, NOW);
        assert!(!m.showable(&none, NOW + 6 * DAY_MS));
        assert!(m.showable(&none, NOW + 7 * DAY_MS), "then 7");
        m.press(MegaphoneCta::Snooze, NOW);
        assert!(m.showable(&none, NOW + 7 * DAY_MS), "and 7 after that");
        // No days given: Desktop's 3.
        m.secondary_cta_data = None;
        assert!(m.showable(&none, NOW + 3 * DAY_MS) && !m.showable(&none, NOW + 2 * DAY_MS));
    }

    #[test]
    fn conditions_as_desktop_checks_them() {
        let mut m = megaphone();
        m.conditional_id = Some("standard_donate".into());
        let old_device = |has_badges| Conditions { device_created_at: Some(NOW - 8 * DAY_MS), has_badges };
        assert!(m.showable(&old_device(Some(false)), NOW));
        assert!(!m.showable(&old_device(Some(true)), NOW), "already a donor");
        assert!(!m.showable(&old_device(None), NOW), "couldn't tell");
        let new_device = Conditions { device_created_at: Some(NOW - DAY_MS), has_badges: Some(false) };
        assert!(!m.showable(&new_device, NOW), "linked this week");
        m.conditional_id = Some("internal_user".into());
        assert!(!m.showable(&old_device(Some(false)), NOW));
        m.conditional_id = None;
        m.primary_cta_id = Some("upgrade".into());
        assert!(!m.showable(&Conditions::default(), NOW), "a button we don't know");
    }

    #[test]
    fn higher_priority_first() {
        let mut low = megaphone();
        low.id = "low".into();
        low.priority = 1;
        let high = megaphone();
        let ids: Vec<_> = visible(&[low, high], &Conditions::default(), NOW).into_iter().map(|v| v.id).collect();
        assert_eq!(ids, ["m1", "low"]);
    }
}
