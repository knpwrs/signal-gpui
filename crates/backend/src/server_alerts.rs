//! What the server warns about as we connect (`X-Signal-Alert`), and our
//! clock against its, as Signal Desktop shows them:
//! `ts/util/handleServerAlerts.preload.ts` (kept with when each came and
//! was dismissed), `getServerAlertToShow`, the critical one's modal timing
//! (three days after it first came, then a day after each dismissal), and
//! `SocketManager`'s clock skew check (`client.maxAllowedClockSkewSeconds`,
//! a day by default, never under an hour).

use std::collections::BTreeMap;

/// Settings key: the alerts (JSON [`ServerAlerts`]), Desktop's
/// `serverAlerts` item.
pub const SETTING_SERVER_ALERTS: &str = "server_alerts";

const HOUR: i64 = 60 * 60 * 1000;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
/// Desktop's `DELAY_BEFORE_SHOWING_MODAL_FIRST_TIME` and `_SUBSEQUENTLY`.
const MODAL_FIRST_DELAY: i64 = 3 * DAY;
const MODAL_AGAIN_DELAY: i64 = DAY;

/// Desktop's `ServerAlert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerAlert {
    /// Open Signal on the phone or the account will be deleted soon.
    CriticalIdlePrimaryDevice,
    /// Open Signal on the phone to keep the account active.
    IdlePrimaryDevice,
}

impl ServerAlert {
    /// Desktop's `parseServerAlertsFromHeader`: unknown ones are left out.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "critical-idle-primary-device" => Some(Self::CriticalIdlePrimaryDevice),
            "idle-primary-device" => Some(Self::IdlePrimaryDevice),
            _ => None,
        }
    }
}

/// When an alert first came and was dismissed (unix millis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AlertInfo {
    pub first_received_at: i64,
    /// The idle warning's close (it can be closed once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<i64>,
    /// The critical one's modal, last closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal_last_dismissed_at: Option<i64>,
}

/// The alerts the server sent last, Desktop's `ServerAlertsType`.
pub type ServerAlerts = BTreeMap<ServerAlert, AlertInfo>;

/// Desktop's `handleServerAlerts`: the alerts this connection gave, each
/// keeping what was known of it; ones not given any more are gone.
pub fn merge(existing: &ServerAlerts, received: &[String], now: i64) -> ServerAlerts {
    received
        .iter()
        .filter_map(|a| ServerAlert::parse(a))
        .map(|alert| (alert, existing.get(&alert).copied().unwrap_or(AlertInfo { first_received_at: now, ..Default::default() })))
        .collect()
}

/// Desktop's `getServerAlertToShow`: the critical one, else the idle one
/// unless it was closed in the last week.
pub fn to_show(alerts: &ServerAlerts, now: i64) -> Option<ServerAlert> {
    if alerts.contains_key(&ServerAlert::CriticalIdlePrimaryDevice) {
        return Some(ServerAlert::CriticalIdlePrimaryDevice);
    }
    let idle = alerts.get(&ServerAlert::IdlePrimaryDevice)?;
    match idle.dismissed_at {
        Some(at) if now - at < WEEK => None,
        _ => Some(ServerAlert::IdlePrimaryDevice),
    }
}

/// When the critical alert's modal shows (Desktop's
/// `maybeShowCriticalIdlePrimaryDeviceModal`): three days after it first
/// came, then a day after each close.
pub fn critical_modal_at(info: &AlertInfo, now: i64) -> i64 {
    if now - info.first_received_at < MODAL_FIRST_DELAY {
        info.first_received_at + MODAL_FIRST_DELAY
    } else {
        match info.modal_last_dismissed_at {
            None => now,
            Some(at) => at + MODAL_AGAIN_DELAY,
        }
    }
}

/// The skew that's too much (Desktop's `#getMaxAllowedSkew`): the remote
/// config's seconds, never under an hour; a day without one.
pub fn max_allowed_skew(config_seconds: Option<i64>) -> i64 {
    match config_seconds {
        Some(seconds) => (seconds * 1000).max(HOUR),
        None => DAY,
    }
}

/// Desktop's `#getSkew`: how far our clock is from the server's, from its
/// time `server` read `elapsed` ms ago.
pub fn skew(server: i64, elapsed: i64, now: i64) -> i64 {
    (server + elapsed - now).abs()
}

/// Where the captcha is solved (Desktop's production `challengeUrl`); it
/// ends by opening `signalcaptcha://<captcha>`.
pub const CAPTCHA_URL: &str = "https://signalcaptchas.org/challenge/generate.html";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_keep_what_was_known_and_drop_the_rest() {
        let received = ["idle-primary-device".to_owned(), "unknown".to_owned()];
        let first = merge(&ServerAlerts::new(), &received, 100);
        assert_eq!(first.len(), 1);
        assert_eq!(first[&ServerAlert::IdlePrimaryDevice].first_received_at, 100);
        let mut dismissed = first.clone();
        dismissed.get_mut(&ServerAlert::IdlePrimaryDevice).unwrap().dismissed_at = Some(200);
        let again = merge(&dismissed, &received, 300);
        assert_eq!(again[&ServerAlert::IdlePrimaryDevice], AlertInfo { first_received_at: 100, dismissed_at: Some(200), modal_last_dismissed_at: None });
        assert!(merge(&again, &[], 400).is_empty());
    }

    #[test]
    fn the_idle_warning_comes_back_a_week_after_closing() {
        let mut alerts = merge(&ServerAlerts::new(), &["idle-primary-device".to_owned()], 0);
        assert_eq!(to_show(&alerts, 10), Some(ServerAlert::IdlePrimaryDevice));
        alerts.get_mut(&ServerAlert::IdlePrimaryDevice).unwrap().dismissed_at = Some(10);
        assert_eq!(to_show(&alerts, 10 + WEEK - 1), None);
        assert_eq!(to_show(&alerts, 10 + WEEK), Some(ServerAlert::IdlePrimaryDevice));
        let both = merge(&alerts, &["idle-primary-device".to_owned(), "critical-idle-primary-device".to_owned()], 20);
        assert_eq!(to_show(&both, 20), Some(ServerAlert::CriticalIdlePrimaryDevice));
    }

    #[test]
    fn the_critical_modal_waits_three_days_then_a_day() {
        let info = AlertInfo { first_received_at: 0, ..Default::default() };
        assert_eq!(critical_modal_at(&info, DAY), 3 * DAY);
        assert_eq!(critical_modal_at(&info, 4 * DAY), 4 * DAY);
        let closed = AlertInfo { modal_last_dismissed_at: Some(4 * DAY), ..info };
        assert_eq!(critical_modal_at(&closed, 4 * DAY + 1), 5 * DAY);
    }

    #[test]
    fn clock_skew_follows_the_remote_config() {
        assert_eq!(max_allowed_skew(None), DAY);
        assert_eq!(max_allowed_skew(Some(60)), HOUR);
        assert_eq!(max_allowed_skew(Some(7200)), 2 * HOUR);
        assert_eq!(skew(1_000, 500, 1_600), 100);
        assert_eq!(skew(1_000, 0, 900), 100);
    }
}
