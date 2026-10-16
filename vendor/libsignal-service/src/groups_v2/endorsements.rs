//! signal-gpui: group send endorsements, as Signal Desktop keeps and uses
//! them (`ts/util/groupSendEndorsements.preload.ts`).
//!
//! The group server hands out one endorsement per full member with the
//! group's state (`GroupResponse.group_send_endorsements_response`). Sending
//! to some members combines theirs into a token the chat server checks in
//! place of their access keys (`Group-Send-Token`).

use std::collections::HashSet;

use libsignal_core::{Aci, ServiceId};
use serde::{Deserialize, Serialize};
use zkgroup::{
    groups::{
        GroupSecretParams, GroupSendEndorsement, GroupSendEndorsementsResponse,
    },
    ServerPublicParams, Timestamp,
};

/// Endorsements expiring sooner than this are refreshed before a send
/// (Desktop's `validateGroupSendEndorsementsExpiration`).
pub const REFRESH_BEFORE_SECONDS: u64 = 2 * 60 * 60;
/// Endorsements expiring later than this are taken as bad and refreshed.
pub const MAX_LIFETIME_SECONDS: u64 = 2 * 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum EndorsementError {
    #[error("the group server sent no endorsements")]
    Empty,
    #[error("couldn't read the endorsements")]
    Deserialization(#[from] zkgroup::ZkGroupDeserializationFailure),
    #[error("the endorsements didn't verify")]
    Verification(#[from] zkgroup::ZkGroupVerificationFailure),
    #[error("no endorsement for {0}")]
    MissingMember(String),
    #[error("the endorsements aren't usable: {0}")]
    Expiration(&'static str),
}

/// One member's endorsement, serialized (compressed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberEndorsement {
    #[serde(with = "crate::utils::serde_aci")]
    pub aci: Aci,
    #[serde(with = "crate::utils::serde_base64")]
    pub endorsement: Vec<u8>,
}

/// A group's endorsements as Desktop stores them
/// (`GroupSendEndorsementsData`): the combined endorsement for every member
/// but us, and each member's own, all sharing one expiration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSendEndorsements {
    /// Unix time, in seconds.
    pub expiration: u64,
    #[serde(with = "crate::utils::serde_base64")]
    pub combined: Vec<u8>,
    pub members: Vec<MemberEndorsement>,
}

impl GroupSendEndorsements {
    /// Desktop's `decodeGroupSendEndorsementsResponse`: verifies the
    /// server's response for `members` (the group's full members, us
    /// included) and keeps one endorsement each, plus the combination of
    /// everyone's but ours.
    pub fn receive(
        response: &[u8],
        members: &[Aci],
        our_aci: Aci,
        group_secret_params: &GroupSecretParams,
        server_public_params: &ServerPublicParams,
        now_seconds: u64,
    ) -> Result<Self, EndorsementError> {
        if response.is_empty() {
            return Err(EndorsementError::Empty);
        }
        let response: GroupSendEndorsementsResponse =
            zkgroup::deserialize(response)?;
        let expiration = response.expiration().epoch_seconds();
        let service_ids: Vec<ServiceId> =
            members.iter().map(|aci| (*aci).into()).collect();
        let received = response.receive_with_service_ids(
            service_ids,
            Timestamp::from_epoch_seconds(now_seconds),
            group_secret_params,
            server_public_params,
        )?;
        let combined = GroupSendEndorsement::combine(
            members
                .iter()
                .zip(&received)
                .filter(|(aci, _)| **aci != our_aci)
                .map(|(_, r)| r.decompressed),
        );
        Ok(Self {
            expiration,
            combined: zkgroup::serialize(&combined),
            members: members
                .iter()
                .zip(&received)
                .map(|(aci, r)| MemberEndorsement {
                    aci: *aci,
                    endorsement: zkgroup::serialize(&r.compressed),
                })
                .collect(),
        })
    }

    /// Desktop's `validateGroupSendEndorsementsExpiration`: usable when
    /// more than two hours and less than two days are left.
    pub fn check_expiration(
        &self,
        now_seconds: u64,
    ) -> Result<(), EndorsementError> {
        if self.expiration <= now_seconds {
            return Err(EndorsementError::Expiration("already expired"));
        }
        let left = self.expiration - now_seconds;
        if left <= REFRESH_BEFORE_SECONDS {
            return Err(EndorsementError::Expiration("expires soon"));
        }
        if left >= MAX_LIFETIME_SECONDS {
            return Err(EndorsementError::Expiration(
                "expires too far in the future",
            ));
        }
        Ok(())
    }

    pub fn has_member(&self, service_id: &ServiceId) -> bool {
        self.members
            .iter()
            .any(|m| ServiceId::from(m.aci) == *service_id)
    }

    /// Desktop's `validateGroupSendEndorsements`: usable now, with an
    /// endorsement for each of `members`.
    pub fn is_valid_for(
        &self,
        members: impl IntoIterator<Item = ServiceId>,
        now_seconds: u64,
    ) -> Result<(), EndorsementError> {
        self.check_expiration(now_seconds)?;
        for member in members {
            if !self.has_member(&member) {
                return Err(EndorsementError::MissingMember(
                    member.service_id_string(),
                ));
            }
        }
        Ok(())
    }

    fn member(
        &self,
        service_id: &ServiceId,
    ) -> Result<GroupSendEndorsement, EndorsementError> {
        let member = self
            .members
            .iter()
            .find(|m| ServiceId::from(m.aci) == *service_id)
            .ok_or_else(|| {
                EndorsementError::MissingMember(service_id.service_id_string())
            })?;
        Ok(zkgroup::deserialize(&member.endorsement)?)
    }

    /// Desktop's `GroupSendEndorsementState.buildToken`: the endorsement for
    /// exactly `recipients`, made the cheapest way (one member's own; the
    /// combined one, less those left out; or theirs combined), as a
    /// serialized `GroupSendFullToken`.
    pub fn build_token(
        &self,
        recipients: &HashSet<ServiceId>,
        our_aci: Aci,
        group_secret_params: &GroupSecretParams,
        now_seconds: u64,
    ) -> Result<Vec<u8>, EndorsementError> {
        self.check_expiration(now_seconds)?;
        let endorsement = self.endorsement_for(recipients, our_aci)?;
        let token = endorsement
            .to_token(group_secret_params)
            .into_full_token(Timestamp::from_epoch_seconds(self.expiration));
        Ok(zkgroup::serialize(&token))
    }

    fn endorsement_for(
        &self,
        recipients: &HashSet<ServiceId>,
        our_aci: Aci,
    ) -> Result<GroupSendEndorsement, EndorsementError> {
        if recipients.len() == 1 {
            let only = recipients.iter().next().expect("one recipient");
            return self.member(only);
        }
        if let Some(missing) = recipients.iter().find(|r| !self.has_member(r))
        {
            return Err(EndorsementError::MissingMember(
                missing.service_id_string(),
            ));
        }
        let ours = ServiceId::from(our_aci);
        let mut others: Vec<ServiceId> = self
            .members
            .iter()
            .map(|m| ServiceId::from(m.aci))
            .filter(|m| !recipients.contains(m))
            .collect();
        let includes_ours = match others.iter().position(|m| *m == ours) {
            Some(at) => {
                others.remove(at);
                false
            },
            None => true,
        };
        let combined = || -> Result<GroupSendEndorsement, EndorsementError> {
            let combined: GroupSendEndorsement =
                zkgroup::deserialize(&self.combined)?;
            if includes_ours {
                Ok(GroupSendEndorsement::combine([
                    combined,
                    self.member(&ours)?,
                ]))
            } else {
                Ok(combined)
            }
        };
        if others.is_empty() {
            return combined();
        }
        // Desktop: `otherMembers.size < memberCount / 2`.
        if others.len() * 2 < self.members.len() {
            let removed = self.combine(others.iter())?;
            return Ok(combined()?.remove(&removed));
        }
        self.combine(recipients.iter())
    }

    fn combine<'a>(
        &self,
        service_ids: impl Iterator<Item = &'a ServiceId>,
    ) -> Result<GroupSendEndorsement, EndorsementError> {
        let endorsements = service_ids
            .map(|s| self.member(s))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GroupSendEndorsement::combine(endorsements))
    }
}
