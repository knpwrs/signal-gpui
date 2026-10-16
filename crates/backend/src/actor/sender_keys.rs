//! Group messages and stories sent with sender keys, as Signal Desktop
//! sends them (`sendContentMessageToGroup`, `ts/util/sendToGroup.preload.ts`),
//! and the group send endorsements group messages go with
//! (`ts/util/groupSendEndorsements.preload.ts`). Everything here runs in the
//! outbound queue, one send at a time, so a target's sender key state is
//! never read and written by two sends at once.

use presage::libsignal_service::groups_v2::endorsements::GroupSendEndorsements;
use presage::libsignal_service::sender::sender_key::SenderKeyInfo;

use super::*;
use crate::sender_keys::{
    StoryBatchPlan, StoryBatchStep, endorsements_setting, endorsements_usable, read_endorsements, read_info,
    sender_key_setting,
};

/// How long after a fetch that got no usable endorsements before fetching
/// again (meanwhile the group's messages go one by one).
const ENDORSEMENT_RETRY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

type GroupSendResult = Result<
    Vec<(ServiceId, Result<(), presage::libsignal_service::sender::MessageSenderError>)>,
    presage::Error<<SqliteStore as presage::store::Store>::Error>,
>;

impl Actor {
    async fn sender_key_info(&self, target: &str) -> SenderKeyInfo {
        let json = self.db.setting(&sender_key_setting(target)).await.ok().flatten();
        read_info(json.as_deref(), now_millis() as u64)
    }

    /// Kept after every send, also one that failed part way: it records
    /// who our key reached.
    async fn save_sender_key_info(&self, target: &str, info: &SenderKeyInfo) {
        let saved = match serde_json::to_string(info) {
            Ok(json) => self.db.set_setting(&sender_key_setting(target), &json).await.map_err(anyhow::Error::from),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = saved {
            warn!(error = %format!("{error:#}"), "couldn't keep the sender key state");
        }
    }

    /// A group message: with our sender key and the group's endorsements to
    /// everyone they cover, one by one to the rest (Desktop's
    /// `sendContentMessageToGroup`). With `only`, to just those members (a
    /// retry to the ones an earlier send missed, Desktop's partial send).
    /// Returns each recipient's result, as presage's
    /// `send_message_to_group_members`; an `Err` means nobody got it.
    // The same error presage's own sends return.
    #[allow(clippy::result_large_err)]
    pub(super) async fn send_to_group(
        &self,
        manager: &mut SignalManager,
        conversation: &ConversationId,
        key: &[u8],
        body: ContentBody,
        timestamp: u64,
        only: Option<&[ServiceId]>,
    ) -> GroupSendResult {
        let target = conversation.to_string();
        let mut info = self.sender_key_info(&target).await;
        let endorsements = self.group_endorsements(manager, conversation, key).await;
        let result = manager
            .send_message_to_group_with_sender_key(key, body, timestamp, only, &mut info, endorsements.as_ref())
            .await;
        self.save_sender_key_info(&target, &info).await;
        let summary = result?;
        match &summary.fell_over {
            Some(reason) => info!(recipients = summary.recipients, %reason, "group message sent one by one"),
            None => info!(
                recipients = summary.recipients,
                sender_key = summary.via_sender_key,
                unregistered = summary.unregistered,
                "group message sent"
            ),
        }
        Ok(summary.results)
    }

    /// The group's send endorsements, fetched again (with the group) when
    /// they're missing, about to expire, or don't cover a member, as
    /// Desktop's `maybeCreateGroupSendEndorsementState` refreshes them. A
    /// fetch that gets none isn't repeated for an hour.
    async fn group_endorsements(
        &self,
        manager: &mut SignalManager,
        conversation: &ConversationId,
        key: &[u8],
    ) -> Option<GroupSendEndorsements> {
        let setting = endorsements_setting(&conversation.to_string());
        let now_secs = now_millis() as u64 / 1000;
        let members = self.group_full_members(key).await;
        let stored = read_endorsements(self.db.setting(&setting).await.ok().flatten().as_deref());
        if let Some(stored) = stored.filter(|e| endorsements_usable(e, &members, now_secs)) {
            return Some(stored);
        }
        let recently_missed = self
            .endorsement_misses
            .borrow()
            .get(conversation)
            .is_some_and(|at| at.elapsed() < ENDORSEMENT_RETRY);
        if recently_missed {
            return None;
        }
        info!("fetching the group's send endorsements");
        let fetched = match manager.group_send_endorsements(key).await {
            Ok(fetched) => fetched.filter(|e| e.check_expiration(now_secs).is_ok()),
            Err(error) => {
                warn!(%error, "couldn't fetch the group's send endorsements");
                None
            }
        };
        match &fetched {
            Some(endorsements) => {
                self.endorsement_misses.borrow_mut().remove(conversation);
                match serde_json::to_string(endorsements) {
                    Ok(json) => {
                        if let Err(error) = self.db.set_setting(&setting, &json).await {
                            warn!(%error, "couldn't keep the group's send endorsements");
                        }
                    }
                    Err(error) => warn!(%error, "couldn't keep the group's send endorsements"),
                }
            }
            None => {
                self.endorsement_misses.borrow_mut().insert(conversation.clone(), std::time::Instant::now());
            }
        }
        fetched
    }

    /// The group's full members (us included), as presage keeps the group.
    async fn group_full_members(&self, key: &[u8]) -> Vec<ServiceId> {
        let Ok(key) = <[u8; 32]>::try_from(key) else { return Vec::new() };
        self.store
            .group(key)
            .await
            .ok()
            .flatten()
            .map(|g| g.members.iter().map(|m| ServiceId::from(m.aci)).collect())
            .unwrap_or_default()
    }

    /// A story to `recipients` with our sender key for `target` (Desktop's
    /// `sendStory` through `sendContentMessageToGroup`); those it can't
    /// reach that way get it one by one, everyone does when the sender key
    /// send fails in a way Desktop falls over on, and nobody more when it
    /// fails otherwise (rate limits, the network, an untrusted identity).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_story_batch(
        &self,
        manager: &mut SignalManager,
        target: &str,
        group: Option<Vec<u8>>,
        recipients: Vec<String>,
        body: ContentBody,
        timestamp: u64,
        job: u64,
    ) {
        let mut ids = Vec::new();
        for recipient in recipients {
            match ServiceId::parse_from_service_id_string(&recipient) {
                Some(id) => ids.push((id, recipient)),
                None => self.story_part_done(job, recipient, Err("not a Signal account".to_owned())),
            }
        }
        // Desktop needs two to send with a sender key.
        if ids.len() < 2 {
            for (_, recipient) in ids {
                self.send_story_part(manager, recipient, body.clone(), timestamp, job).await;
            }
            return;
        }
        let service_ids: Vec<ServiceId> = ids.iter().map(|(id, _)| *id).collect();
        let mut info = self.sender_key_info(target).await;
        let result = manager
            .send_story_with_sender_key(&service_ids, body.clone(), timestamp, &mut info, group.as_deref())
            .await;
        self.save_sender_key_info(target, &info).await;
        let plan = match result {
            Ok(Ok(outcome)) => {
                info!(
                    sent = outcome.sent.len(),
                    one_by_one = outcome.normal.len(),
                    unregistered = outcome.unregistered.len(),
                    "story sent with a sender key"
                );
                StoryBatchPlan::from_outcome(&outcome)
            }
            Ok(Err(error)) if error.sender_key_may_fall_over() => {
                info!(%error, "story sender key send failed; sending one by one");
                StoryBatchPlan::all_one_by_one()
            }
            Ok(Err(error)) => StoryBatchPlan::failed(super::stories::failure_reason(&error.to_string())),
            Err(error) => StoryBatchPlan::failed(super::stories::failure_reason(&error.to_string())),
        };
        for (id, recipient) in ids {
            match plan.for_recipient(&id) {
                StoryBatchStep::Done(result) => self.story_part_done(job, recipient, result),
                StoryBatchStep::OneByOne => {
                    self.send_story_part(manager, recipient, body.clone(), timestamp, job).await
                }
            }
        }
    }
}
