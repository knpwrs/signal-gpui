//! Sending to many at once with sender keys, as Signal Desktop does
//! (`ts/util/sendToGroup.preload.ts`): where each group's and story list's
//! sender key state and each group's send endorsements are kept, and which
//! story list sends a story to whom.
//!
//! The sending itself is in libsignal-service (`sender::sender_key`, see
//! vendor/README.md); the actor's side is in `actor/sender_keys.rs`.

use presage::libsignal_service::{
    groups_v2::endorsements::GroupSendEndorsements, protocol::ServiceId, sender::sender_key::SenderKeyInfo,
};

use crate::StoryList;

/// Settings key: our sender key state for a group (`target` is its
/// conversation id) or a story list (`list:<id>`), as JSON
/// [`SenderKeyInfo`] (Desktop keeps it on the conversation or the
/// distribution list as `senderKeyInfo`).
pub(crate) fn sender_key_setting(target: &str) -> String {
    format!("sender_key.{target}")
}

/// The sender key target for a story list.
pub(crate) fn list_target(list_id: &str) -> String {
    format!("list:{list_id}")
}

/// Settings key: a group's send endorsements, as JSON
/// [`GroupSendEndorsements`] (Desktop's `groupSendCombinedEndorsement` and
/// `groupSendMemberEndorsement` tables).
pub(crate) fn endorsements_setting(conversation: &str) -> String {
    format!("group_send_endorsements.{conversation}")
}

/// Our sender key state from its setting; a new one when there's none (or
/// it can't be read: the worst that does is send our key again).
pub(crate) fn read_info(json: Option<&str>, now_ms: u64) -> SenderKeyInfo {
    json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_else(|| SenderKeyInfo::new(now_ms))
}

pub(crate) fn read_endorsements(json: Option<&str>) -> Option<GroupSendEndorsements> {
    json.and_then(|j| serde_json::from_str(j).ok())
}

/// Desktop's `validateGroupSendEndorsements`: endorsements we can send with
/// now, covering every one of the group's full `members` (us included).
pub(crate) fn endorsements_usable(endorsements: &GroupSendEndorsements, members: &[ServiceId], now_secs: u64) -> bool {
    endorsements.is_valid_for(members.iter().copied(), now_secs).is_ok()
}

/// One story list's share of a story: who it goes to through that list,
/// and whether they may reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListSend {
    pub list_id: String,
    pub allows_replies: bool,
    pub recipients: Vec<String>,
}

/// Desktop's `sendStoryMessage`: the lists that allow replies first, and
/// each person sent the story through the first list they're in, so nobody
/// gets it twice. Lists left with nobody are dropped.
pub(crate) fn list_sends(lists: &[(StoryList, Vec<String>)]) -> Vec<ListSend> {
    let mut ordered: Vec<&(StoryList, Vec<String>)> = lists.iter().collect();
    // Stable: lists keep their order otherwise.
    ordered.sort_by_key(|(list, _)| !list.allows_replies);
    let mut seen = std::collections::HashSet::new();
    ordered
        .into_iter()
        .map(|(list, members)| ListSend {
            list_id: list.id.clone(),
            allows_replies: list.allows_replies,
            recipients: members.iter().filter(|m| seen.insert(m.as_str())).cloned().collect(),
        })
        .filter(|send| !send.recipients.is_empty())
        .collect()
}

/// What becomes of each recipient of a story sent with a sender key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoryBatchStep {
    /// Finished: reached, or why not.
    Done(Result<(), String>),
    /// Still to send to on their own.
    OneByOne,
}

/// How a story's sender key send went, for its recipients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoryBatchPlan {
    /// It went: those reached are done, those not registered are failures,
    /// and everyone else (no session, our key didn't reach them) goes one
    /// by one.
    Sent { sent: Vec<ServiceId>, unregistered: Vec<ServiceId> },
    /// It failed in a way Desktop falls over on: everyone one by one.
    AllOneByOne,
    /// It failed otherwise: nobody more is sent to, and each says why.
    Failed(String),
}

impl StoryBatchPlan {
    pub(crate) fn from_outcome(outcome: &presage::libsignal_service::sender::sender_key::SenderKeyOutcome) -> Self {
        Self::Sent { sent: outcome.sent.clone(), unregistered: outcome.unregistered.clone() }
    }

    pub(crate) fn all_one_by_one() -> Self {
        Self::AllOneByOne
    }

    pub(crate) fn failed(reason: String) -> Self {
        Self::Failed(reason)
    }

    pub(crate) fn for_recipient(&self, recipient: &ServiceId) -> StoryBatchStep {
        match self {
            Self::Sent { sent, .. } if sent.contains(recipient) => StoryBatchStep::Done(Ok(())),
            Self::Sent { unregistered, .. } if unregistered.contains(recipient) => {
                StoryBatchStep::Done(Err("no longer on Signal".to_owned()))
            }
            Self::Sent { .. } | Self::AllOneByOne => StoryBatchStep::OneByOne,
            Self::Failed(reason) => StoryBatchStep::Done(Err(reason.clone())),
        }
    }
}

#[cfg(test)]
mod tests;
