//! The server's alerts and our clock, read as the authenticated websocket
//! opens (see `crate::server_alerts` for Desktop's rules).

use crate::server_alerts::{self, SETTING_SERVER_ALERTS, ServerAlert, ServerAlerts};

use super::*;

impl Actor {
    async fn saved_alerts(&self) -> ServerAlerts {
        self.db
            .setting(SETTING_SERVER_ALERTS)
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    async fn save_alerts(&self, alerts: &ServerAlerts) {
        match serde_json::to_string(alerts) {
            Ok(json) => {
                if let Err(error) = self.db.set_setting(SETTING_SERVER_ALERTS, &json).await {
                    warn!(%error, "couldn't save server alerts");
                }
            }
            Err(error) => warn!(%error, "couldn't encode server alerts"),
        }
    }

    /// The authenticated websocket opened: its alerts replace the last
    /// ones (Desktop's `handleServerAlerts`), and its clock is checked.
    pub(super) async fn on_connected(self: &Rc<Self>) {
        let Some(info) = presage::libsignal_service::websocket::connection_info::take() else { return };
        let alerts = server_alerts::merge(&self.saved_alerts().await, &info.alerts, now_millis());
        self.save_alerts(&alerts).await;
        self.show_alerts(alerts);
        if let Some((server, read_at)) = info.server_timestamp {
            self.check_clock(server as i64, read_at);
        }
    }

    /// Shows the alerts, and schedules the critical one's modal.
    fn show_alerts(self: &Rc<Self>, alerts: ServerAlerts) {
        if let Some(task) = self.alert_modal.borrow_mut().take() {
            task.abort();
        }
        if let Some(info) = alerts.get(&ServerAlert::CriticalIdlePrimaryDevice).copied() {
            let wait = (server_alerts::critical_modal_at(&info, now_millis()) - now_millis()).max(0);
            let this = self.clone();
            *self.alert_modal.borrow_mut() = Some(tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(wait as u64)).await;
                this.emit(Event::CriticalAlertModal);
            }));
        }
        self.emit(Event::ServerAlerts(alerts));
    }

    /// At startup: the alerts the server sent last show until it says again.
    pub(super) async fn resume_alerts(self: &Rc<Self>) {
        let alerts = self.saved_alerts().await;
        if !alerts.is_empty() {
            self.show_alerts(alerts);
        }
    }

    /// The idle warning's close (once), or the critical modal's.
    pub(super) async fn dismiss_server_alert(self: &Rc<Self>, alert: ServerAlert) {
        let mut alerts = self.saved_alerts().await;
        let Some(info) = alerts.get_mut(&alert) else { return };
        let now = now_millis();
        match alert {
            ServerAlert::IdlePrimaryDevice if info.dismissed_at.is_none() => info.dismissed_at = Some(now),
            ServerAlert::CriticalIdlePrimaryDevice => info.modal_last_dismissed_at = Some(now),
            ServerAlert::IdlePrimaryDevice => return,
        }
        self.save_alerts(&alerts).await;
        self.show_alerts(alerts);
    }

    /// Desktop's `#maybeUpdateSkewState`: our clock against the server's,
    /// against `client.maxAllowedClockSkewSeconds`.
    pub(super) fn check_clock(&self, server: i64, read_at: Instant) {
        let max = server_alerts::max_allowed_skew(crate::remote_config().int("client.maxAllowedClockSkewSeconds"));
        let skewed = server_alerts::skew(server, read_at.elapsed().as_millis() as i64, now_millis()) > max;
        if self.clock_skewed.replace(skewed) != skewed {
            if skewed {
                warn!("our clock is too far from the server's");
            }
            self.emit(Event::ClockSkew(skewed));
        }
    }
}
