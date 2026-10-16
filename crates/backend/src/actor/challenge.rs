//! Captcha challenges, as Signal Desktop's `ChallengeHandler`
//! (`ts/challenge.dom.ts`) answers a 428 on send: the messages it stopped
//! are held ("Send paused"), the captcha dialog asks for verification
//! (`CaptchaDialog`), the token from `signalcaptcha://` goes to the server
//! (`PUT /v1/challenge`), and once it's accepted the held messages are
//! sent again. Kept for a day, as Desktop keeps registered challenges
//! (`DEFAULT_EXPIRE_AFTER`), so a restart shows the dialog again. Skipping
//! (Desktop's "Skip verification") or a refused answer leaves the held
//! messages failed, to be sent again with Retry Send.

use super::*;

/// Settings key: the challenge waiting for a captcha (JSON [`Challenge`]).
const SETTING_CHALLENGE: &str = "challenge";
/// Desktop forgets a challenge after a day.
const CHALLENGE_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

/// A challenge waiting for its captcha: the server's token, the messages
/// it held, and when it came.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Challenge {
    pub token: String,
    pub held: Vec<i64>,
    pub created_at: i64,
}

impl Actor {
    async fn saved_challenge(&self) -> Option<Challenge> {
        let json = self.db.setting(SETTING_CHALLENGE).await.ok().flatten().filter(|j| !j.is_empty())?;
        serde_json::from_str::<Challenge>(&json).ok().filter(|c| now_millis() - c.created_at < CHALLENGE_LIFETIME_MS)
    }

    async fn save_challenge(&self, challenge: Option<&Challenge>) {
        let json = challenge.and_then(|c| serde_json::to_string(c).ok()).unwrap_or_default();
        if let Err(error) = self.db.set_setting(SETTING_CHALLENGE, &json).await {
            warn!(%error, "couldn't save the captcha challenge");
        }
    }

    /// A send of message `id` got a 428: it's held with the challenge
    /// (the newest token wins), and the dialog asks.
    pub(super) async fn hold_for_challenge(&self, conversation: &ConversationId, id: i64, token: &str) -> DeliveryStatus {
        let _lock = self.challenge_lock.lock().await;
        let mut challenge =
            self.saved_challenge().await.unwrap_or(Challenge { token: token.to_owned(), held: Vec::new(), created_at: now_millis() });
        challenge.token = token.to_owned();
        if !challenge.held.contains(&id) {
            challenge.held.push(id);
        }
        self.save_challenge(Some(&challenge)).await;
        if let Err(error) = self.db.set_message_status(id, DeliveryStatus::Paused).await {
            self.error("failed to pause a message", error);
        }
        self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
        self.emit(Event::CaptchaRequired);
        DeliveryStatus::Paused
    }

    /// At startup: a challenge still waiting asks again.
    pub(super) async fn resume_challenge(&self) {
        if self.saved_challenge().await.is_some() {
            self.emit(Event::CaptchaRequired);
        }
    }

    /// The captcha's answer (`signalcaptcha://…`): sent to the server, and
    /// the held messages go again once it's accepted.
    pub(super) async fn submit_captcha(self: Rc<Self>, captcha: String) {
        // Held until the challenge is cleared, so a message held meanwhile
        // isn't lost; the resends go after.
        let lock = self.challenge_lock.lock().await;
        let Some(challenge) = self.saved_challenge().await else {
            return self.emit(Event::Captcha(Ok(())));
        };
        let Some(manager) = self.manager() else {
            return self.emit(Event::Captcha(Err("Not connected".into())));
        };
        let captcha = captcha.trim().trim_end_matches('/').to_owned();
        match manager.submit_recaptcha_challenge(&challenge.token, &captcha).await {
            Ok(()) => {
                info!(held = challenge.held.len(), "captcha accepted; sending what it held");
                self.save_challenge(None).await;
                drop(lock);
                self.emit(Event::Captcha(Ok(())));
                for id in challenge.held {
                    self.clone().retry_send(id).await;
                }
            }
            Err(error) => {
                // Desktop puts its dialog away and lets the sends fail, to be
                // tried again (which may ask again).
                warn!(%error, "captcha refused");
                self.release_held(challenge).await;
                self.emit(Event::Captcha(Err(format!("{error}"))));
            }
        }
    }

    /// Desktop's "Skip verification": the held messages fail.
    pub(super) async fn skip_captcha(&self) {
        let _lock = self.challenge_lock.lock().await;
        if let Some(challenge) = self.saved_challenge().await {
            self.release_held(challenge).await;
        }
    }

    async fn release_held(&self, challenge: Challenge) {
        self.save_challenge(None).await;
        for id in challenge.held {
            let Ok(Some(message)) = self.db.message(id).await else { continue };
            if message.status != DeliveryStatus::Paused {
                continue;
            }
            if let Err(error) = self.db.set_message_status(id, DeliveryStatus::Failed).await {
                self.error("failed to update a held message", error);
            }
            self.emit(Event::MessageChanged { conversation: message.conversation_id, message_id: id });
        }
    }
}
