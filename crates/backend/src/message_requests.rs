//! Answering message requests: the notices an answer leaves, and spam
//! reports, as Signal Desktop sends them (`ts/jobs/reportSpamJobQueue.preload.ts`,
//! `reportMessage` in `ts/textsecure/WebAPI.preload.ts`): one
//! `POST /v1/messages/report/{aci}/{serverGuid}` per message GUID, with the
//! sender's report spam token in the body when they gave one, and Desktop's
//! rules for when to try again.

use std::time::Duration;

use anyhow::Context as _;
use presage::libsignal_service::{configuration::Endpoint, push_service::HttpAuthOverride};
use signal_store::{MessageRequestEvent, MessageRequestResponse};

use crate::actor::SignalManager;

/// How long Desktop waits before trying a report again (`RETRY_WAIT_TIME`).
pub const RETRY_WAIT: Duration = Duration::from_secs(60);
/// How many times Desktop tries (`maxAttempts`).
pub const MAX_ATTEMPTS: u32 = 25;

/// The request for one reported message: its path and JSON body. Desktop
/// sends `{ token }`, which leaves the key out when there's no token.
pub fn report_request(aci: &str, server_guid: &str, token: Option<&str>) -> (String, serde_json::Value) {
    let path = format!("/v1/messages/report/{aci}/{server_guid}");
    let body = match token {
        Some(token) => serde_json::json!({ "token": token }),
        None => serde_json::json!({}),
    };
    (path, body)
}

/// What to do after the server answered a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOutcome {
    Done,
    /// Wait [`RETRY_WAIT`] and send it again.
    Retry,
    /// Desktop gives up (508, and the other 4xx answers).
    GiveUp,
}

/// Desktop's `RETRYABLE_4XX_FAILURE_STATUSES`.
const RETRYABLE_4XX: [u16; 15] = [404, 408, 410, 412, 413, 414, 417, 423, 424, 425, 426, 428, 429, 431, 449];

impl ReportOutcome {
    /// Desktop's handling of a report's HTTP status (`None`: no answer, a
    /// network failure, which its job queue tries again).
    pub fn of(status: Option<u16>) -> Self {
        match status {
            None => Self::Retry,
            Some(code) if code < 400 => Self::Done,
            Some(508) => Self::GiveUp,
            Some(code) if RETRYABLE_4XX.contains(&code) || (500..=599).contains(&code) => Self::Retry,
            Some(_) => Self::GiveUp,
        }
    }
}

/// The notices an answer leaves, as Desktop's `applyMessageRequestResponse`
/// adds them: accepted (or unblocked, when it was blocked), blocked,
/// reported. None when the answer is the one we had.
pub fn answer_events(before: MessageRequestResponse, was_blocked: bool, answer: MessageRequestResponse) -> Vec<MessageRequestEvent> {
    let mut events = Vec::new();
    if answer == before {
        return events;
    }
    if answer == MessageRequestResponse::Accept {
        events.push(if was_blocked { MessageRequestEvent::Unblock } else { MessageRequestEvent::Accept });
    }
    if answer.blocks() && !was_blocked {
        events.push(MessageRequestEvent::Block);
    }
    if answer.spam() && !before.spam() {
        events.push(MessageRequestEvent::Spam);
    }
    events
}

/// Reports one message, trying again as Desktop's job queue does.
pub(crate) async fn report(manager: &SignalManager, aci: &str, server_guid: &str, token: Option<&str>) -> anyhow::Result<()> {
    let (path, body) = report_request(aci, server_guid, token);
    let service = crate::storage::push_service(manager)?;
    for attempt in 1..=MAX_ATTEMPTS {
        let answer = service
            .request(reqwest::Method::POST, Endpoint::service(path.clone()), HttpAuthOverride::NoOverride)?
            .json(&body)
            .send()
            .await;
        let status = answer.as_ref().ok().map(|r| r.status().as_u16());
        match ReportOutcome::of(status) {
            ReportOutcome::Done => return Ok(()),
            ReportOutcome::GiveUp => anyhow::bail!("the server answered {}", status.unwrap_or_default()),
            ReportOutcome::Retry if attempt < MAX_ATTEMPTS => {
                tracing::info!(?status, attempt, "spam report not taken yet; trying again in a minute");
                tokio::time::sleep(RETRY_WAIT).await;
            }
            ReportOutcome::Retry => {}
        }
    }
    Err(anyhow::anyhow!("gave up after {MAX_ATTEMPTS} tries")).context("reporting spam")
}

#[cfg(test)]
mod tests {
    use super::*;
    use signal_store::{MessageRequestEvent as E, MessageRequestResponse as R};

    #[test]
    fn a_report_names_the_sender_and_message_and_carries_the_token() {
        let (path, body) = report_request("aaaa-bbbb", "1234-5678", Some("dG9rZW4="));
        assert_eq!(path, "/v1/messages/report/aaaa-bbbb/1234-5678");
        assert_eq!(body.to_string(), r#"{"token":"dG9rZW4="}"#);
        let (_, body) = report_request("aaaa-bbbb", "1234-5678", None);
        assert_eq!(body.to_string(), "{}");
    }

    #[test]
    fn retries_follow_desktop() {
        assert_eq!(ReportOutcome::of(Some(200)), ReportOutcome::Done);
        assert_eq!(ReportOutcome::of(Some(204)), ReportOutcome::Done);
        assert_eq!(ReportOutcome::of(None), ReportOutcome::Retry);
        assert_eq!(ReportOutcome::of(Some(429)), ReportOutcome::Retry);
        assert_eq!(ReportOutcome::of(Some(404)), ReportOutcome::Retry);
        assert_eq!(ReportOutcome::of(Some(503)), ReportOutcome::Retry);
        assert_eq!(ReportOutcome::of(Some(508)), ReportOutcome::GiveUp);
        assert_eq!(ReportOutcome::of(Some(400)), ReportOutcome::GiveUp);
        assert_eq!(ReportOutcome::of(Some(403)), ReportOutcome::GiveUp);
    }

    #[test]
    fn answers_leave_desktops_notices() {
        assert_eq!(answer_events(R::Unknown, false, R::Accept), [E::Accept]);
        assert_eq!(answer_events(R::Block, true, R::Accept), [E::Unblock]);
        assert_eq!(answer_events(R::Accept, false, R::Accept), []);
        assert_eq!(answer_events(R::Unknown, false, R::BlockAndSpam), [E::Block, E::Spam]);
        assert_eq!(answer_events(R::Spam, false, R::BlockAndSpam), [E::Block]);
        assert_eq!(answer_events(R::Unknown, true, R::Block), []);
        assert_eq!(answer_events(R::Unknown, false, R::Delete), []);
        assert_eq!(answer_events(R::Unknown, false, R::Spam), [E::Spam]);
    }
}
