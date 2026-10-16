//! signal-gpui: what the server says as the authenticated websocket opens,
//! kept for the app to take: its alerts (`X-Signal-Alert`, a comma
//! separated list such as `idle-primary-device`) and its clock
//! (`X-Signal-Timestamp`, unix millis), with when that was read.

use std::sync::Mutex;
use std::time::Instant;

/// What the last authenticated connection's response said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// Each `X-Signal-Alert` value, split at commas, trimmed, lowercase.
    pub alerts: Vec<String>,
    /// The server's time and when we read it.
    pub server_timestamp: Option<(u64, Instant)>,
}

static LAST: Mutex<Option<ConnectionInfo>> = Mutex::new(None);

pub(crate) fn record(headers: &reqwest::header::HeaderMap) {
    let info = parse(headers);
    if let Ok(mut last) = LAST.lock() {
        *last = Some(info);
    }
}

fn parse(headers: &reqwest::header::HeaderMap) -> ConnectionInfo {
    let alerts = headers
        .get_all("x-signal-alert")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    let server_timestamp = headers
        .get("x-signal-timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|t| (t, Instant::now()));
    ConnectionInfo { alerts, server_timestamp }
}

/// Takes what the last authenticated connection said, once.
pub fn take() -> Option<ConnectionInfo> {
    LAST.lock().ok().and_then(|mut last| last.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_and_the_clock_are_read() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append("x-signal-alert", "Critical-Idle-Primary-Device, idle-primary-device".parse().unwrap());
        headers.append("x-signal-alert", "something-new".parse().unwrap());
        headers.insert("x-signal-timestamp", "1700000000000".parse().unwrap());
        let info = parse(&headers);
        assert_eq!(info.alerts, ["critical-idle-primary-device", "idle-primary-device", "something-new"]);
        assert_eq!(info.server_timestamp.map(|(t, _)| t), Some(1_700_000_000_000));
        assert_eq!(parse(&reqwest::header::HeaderMap::new()), ConnectionInfo::default());
    }
}
