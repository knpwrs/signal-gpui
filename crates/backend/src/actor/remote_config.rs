//! Fetching the remote config on Desktop's schedule (`ts/RemoteConfig.dom.ts`
//! and `ts/background.preload.ts`): once when the app connects, before the
//! message queue, and when the window becomes active, at most every two
//! hours. See `crate::remote_config` for what's in it.

use super::*;
use crate::remote_config::{self, RemoteConfig};

/// Desktop's `maybeRefreshRemoteConfig` throttle.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
/// A first fetch that takes longer than this doesn't hold up the queue.
const FIRST_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

impl Actor {
    /// Puts the saved config in effect (Desktop's
    /// `restoreRemoteConfigFromStorage`).
    pub(super) async fn load_remote_config(&self) {
        if let Some(config) = remote_config::load(&self.db).await {
            remote_config::set_current(config);
        }
    }

    /// The fetch when the app connects (Desktop's `forceRefreshRemoteConfig`
    /// after the first authenticated connect), bounded so a slow server
    /// doesn't hold up the message queue.
    pub(super) async fn first_remote_config(&self, manager: &SignalManager) {
        if tokio::time::timeout(FIRST_FETCH_TIMEOUT, self.refresh_remote_config(manager)).await.is_err() {
            warn!("remote config: the first fetch timed out");
        }
    }

    /// The window became active: fetches again unless this did in the last
    /// two hours (Desktop's leading-edge throttle; the first fetch, like
    /// Desktop's forced one, doesn't count).
    pub(super) async fn maybe_refresh_remote_config(&self) {
        if self.remote_config_at.get().is_some_and(|at| at.elapsed() < REFRESH_INTERVAL) {
            return;
        }
        let Some(manager) = self.manager() else { return };
        self.remote_config_at.set(Some(Instant::now()));
        self.refresh_remote_config(&manager).await;
    }

    /// Fetches the config and, when it changed, saves it and puts it in
    /// effect. Failures keep the last config.
    async fn refresh_remote_config(&self, manager: &SignalManager) {
        let etag = self.db.setting(remote_config::SETTING_ETAG).await.ok().flatten();
        let response = match manager.remote_config(etag.as_deref()).await {
            Ok(response) => response,
            Err(error) => return warn!(%error, "remote config: fetch failed"),
        };
        if let Some(server) = response.server_timestamp {
            let skew = server as i64 - now_millis();
            if skew.abs() > 60 * 60 * 1000 {
                warn!(server, skew, "remote config: severe clock skew");
            }
        }
        let Some(values) = response.config else {
            return debug!("remote config: unchanged");
        };
        let config = RemoteConfig::from_server(values);
        let json = match serde_json::to_string(&config) {
            Ok(json) => json,
            Err(error) => return warn!(%error, "remote config: couldn't encode"),
        };
        // The ETag only once the config it stands for is saved: otherwise
        // the next fetch would get a 304 for a config we don't have.
        if let Err(error) = self.db.set_setting(remote_config::SETTING_CONFIG, &json).await {
            return warn!(%error, "remote config: couldn't save");
        }
        if let Some(etag) = &response.etag
            && let Err(error) = self.db.set_setting(remote_config::SETTING_ETAG, etag).await
        {
            warn!(%error, "remote config: couldn't save its ETag");
        }
        if remote_config::set_current(config) {
            info!("remote config changed");
            self.emit(Event::RemoteConfigChanged);
        }
    }
}
