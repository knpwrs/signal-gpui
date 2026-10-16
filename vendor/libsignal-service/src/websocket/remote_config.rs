//! signal-gpui: the account's remote config (`GET /v2/config`), as Signal
//! Desktop's `getConfig` in `ts/textsecure/WebAPI.preload.ts` fetches it.

use std::collections::HashMap;

use reqwest::{Method, StatusCode};
use serde::Deserialize;

use crate::{
    content::ServiceError,
    push_service::response::SignalServiceResponse,
    websocket::{self, SignalWebSocket},
};

/// An answer to `GET /v2/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigResponse {
    /// Every key the server set for this account and its value, or `None`
    /// when the config hasn't changed since the `ETag` given (a 304).
    pub config: Option<HashMap<String, String>>,
    /// The server's clock (`X-Signal-Timestamp`), in Unix millis.
    pub server_timestamp: Option<u64>,
    /// The config's hash (`ETag`), to send back as `If-None-Match`.
    pub etag: Option<String>,
}

#[derive(Deserialize)]
struct ConfigBody {
    config: HashMap<String, String>,
}

impl SignalWebSocket<websocket::Identified> {
    /// Fetches the remote config; with `etag` (the last answer's), a config
    /// that hasn't changed comes back as `config: None`.
    pub async fn get_remote_config(
        &mut self,
        etag: Option<&str>,
    ) -> Result<RemoteConfigResponse, ServiceError> {
        let mut request = self.http_request(Method::GET, "/v2/config")?;
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let response = request.send().await?;
        let server_timestamp = response
            .header("x-signal-timestamp")
            .and_then(|t| t.parse().ok());
        let etag = response.header("etag").map(str::to_owned);
        if response.status_code() == Some(StatusCode::NOT_MODIFIED) {
            return Ok(RemoteConfigResponse {
                config: None,
                server_timestamp,
                etag,
            });
        }
        let body: ConfigBody =
            response.service_error_for_status().await?.json().await?;
        Ok(RemoteConfigResponse {
            config: Some(body.config),
            server_timestamp,
            etag,
        })
    }
}
