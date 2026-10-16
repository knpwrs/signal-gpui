//! The actions of one group change, as Desktop builds them
//! (`ts/groups.preload.ts`: `buildDeleteMemberChange`,
//! `buildModifyMemberRoleChange`, `buildNewGroupLinkChange` and the rest),
//! before the group server signs the change. Pure functions of the group as
//! the server has it now, so their wire format is checked against zkgroup
//! with made-up keys (see the tests); [`crate::groups::GroupSession`] sends
//! them.

use anyhow::{anyhow, bail};
use presage::libsignal_service::{
    groups_v2::{AccessRequired, Group, Role},
    prelude::ProtobufMessage as _,
    proto::{self, group_change::Actions, group_change::actions as action},
    protocol::{Aci, Pni, ServiceId},
    zkgroup::{
        self, ServerPublicParams,
        groups::GroupSecretParams,
        profiles::ExpiringProfileKeyCredential,
    },
};

/// A group link's password length (Desktop's
/// `GROUP_INVITE_LINK_PASSWORD_LENGTH`).
pub const LINK_PASSWORD_LENGTH: usize = 16;

#[cfg(test)]
fn secret_params(master_key: [u8; 32]) -> GroupSecretParams {
    GroupSecretParams::derive_from_master_key(presage::libsignal_service::zkgroup::groups::GroupMasterKey::new(master_key))
}

/// A service id encrypted under the group's key, as change actions name
/// people (Desktop's `encryptServiceId`).
pub fn encrypt_id(secret: &GroupSecretParams, id: ServiceId) -> Vec<u8> {
    zkgroup::serialize(&secret.encrypt_service_id(id))
}

fn is_member(group: &Group, aci: Aci) -> bool {
    group.members.iter().any(|m| m.aci == aci)
}

fn is_requesting(group: &Group, aci: Aci) -> bool {
    group.members_pending_admin_approval.iter().any(|m| m.aci == aci)
}

/// Desktop's `_maybeBuildAddBannedMemberActions`: whoever an admin removes
/// or turns down is banned from rejoining by the link (never us, and not
/// twice). The oldest bans go to keep the list under the group size limit.
fn ban(secret: &GroupSecretParams, group: &Group, me: Aci, target: ServiceId, limit: usize, actions: &mut Actions) {
    if target == ServiceId::from(me) || group.members_banned.iter().any(|b| b.user_id == target) {
        return;
    }
    let mut banned: Vec<_> = group.members_banned.iter().collect();
    banned.sort_by_key(|b| std::cmp::Reverse(b.timestamp));
    for old in banned.into_iter().skip(limit.saturating_sub(1)) {
        actions.delete_members_banned.push(action::DeleteMemberBannedAction { deleted_user_id: encrypt_id(secret, old.user_id) });
    }
    actions.add_members_banned.push(action::AddMemberBannedAction {
        added: Some(proto::MemberBanned { user_id: encrypt_id(secret, target), timestamp: 0 }),
    });
}

/// Desktop's `buildDeleteMemberChange`: removes a member and bans them.
pub fn remove_member(
    secret: &GroupSecretParams,
    group: &Group,
    me: Aci,
    target: Aci,
    limit: usize,
    actions: &mut Actions,
) -> anyhow::Result<()> {
    if !is_member(group, target) {
        bail!("they're not in the group");
    }
    actions.delete_members.push(action::DeleteMemberAction { deleted_user_id: encrypt_id(secret, target.into()) });
    ban(secret, group, me, target.into(), limit, actions);
    Ok(())
}

/// Desktop's `buildModifyMemberRoleChange`: makes a member an admin, or not.
/// An admin's label goes with their admin role when only admins may have
/// labels.
pub fn set_role(secret: &GroupSecretParams, group: &Group, target: Aci, admin: bool, actions: &mut Actions) -> anyhow::Result<()> {
    let member = group.members.iter().find(|m| m.aci == target).ok_or_else(|| anyhow!("they're not in the group"))?;
    let user_id = encrypt_id(secret, target.into());
    let labels_admins_only = group.access_control.as_ref().is_some_and(|a| a.member_label == AccessRequired::Administrator);
    let drop_label = !admin && member.role == Role::Administrator && labels_admins_only && member.label.is_some();
    if drop_label {
        actions.modify_member_labels.push(action::ModifyMemberLabelAction {
            user_id: user_id.clone(),
            label_emoji: Vec::new(),
            label_string: Vec::new(),
        });
    }
    let role = if admin { Role::Administrator } else { Role::Default };
    actions.modify_member_roles.push(action::ModifyMemberRoleAction { user_id, role: role.into() });
    Ok(())
}

/// One of Desktop's group permissions (`GroupV2Permissions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Permission {
    /// Who can edit the name, photo, description and timer.
    EditInfo { admins_only: bool },
    /// Who can add members.
    AddMembers { admins_only: bool },
    /// Who can send messages (`announcementsOnly`).
    SendMessages { admins_only: bool },
}

/// Desktop's `buildAccessControlAttributesChange`,
/// `buildAccessControlMembersChange` and `buildAnnouncementsOnlyChange`.
pub fn set_permission(permission: Permission, actions: &mut Actions) {
    let access = |admins_only: bool| i32::from(if admins_only { AccessRequired::Administrator } else { AccessRequired::Member });
    match permission {
        Permission::EditInfo { admins_only } => {
            actions.modify_attributes_access =
                Some(action::ModifyAttributesAccessControlAction { attributes_access: access(admins_only) })
        }
        Permission::AddMembers { admins_only } => {
            actions.modify_member_access = Some(action::ModifyMembersAccessControlAction { members_access: access(admins_only) })
        }
        Permission::SendMessages { admins_only } => {
            actions.modify_announcements_only = Some(action::ModifyAnnouncementsOnlyAction { announcements_only: admins_only })
        }
    }
}

/// A change to a group's link (Desktop's `GroupLinkManagement`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LinkChange {
    /// Turn it on, without admin approval (Desktop's `toggleGroupLink(true)`).
    On,
    Off,
    /// A new password: the old link stops working (`refreshGroupLink`).
    Reset,
    /// Whether an admin approves who joins (`updateAccessControlAddFromInviteLink`).
    Approval(bool),
}

/// Whether a link access lets anyone join (Desktop's `isAccessControlEnabled`).
pub fn link_enabled(access: AccessRequired) -> bool {
    matches!(access, AccessRequired::Any | AccessRequired::Administrator)
}

/// The actions for a link change; `new_password` is used when one's needed
/// (turning on a group that never had a link, or a reset).
pub fn change_link(group: &Group, change: LinkChange, new_password: [u8; LINK_PASSWORD_LENGTH], actions: &mut Actions) {
    let set_access = |actions: &mut Actions, access: AccessRequired| {
        actions.modify_add_from_invite_link_access =
            Some(action::ModifyAddFromInviteLinkAccessControlAction { add_from_invite_link_access: access.into() })
    };
    let set_password = |actions: &mut Actions| {
        actions.modify_invite_link_password =
            Some(action::ModifyInviteLinkPasswordAction { invite_link_password: new_password.to_vec() })
    };
    match change {
        LinkChange::On => {
            // Desktop's `buildNewGroupLinkChange` when there's no password yet.
            if group.invite_link_password.is_empty() {
                set_password(actions);
            }
            set_access(actions, AccessRequired::Any);
        }
        LinkChange::Off => set_access(actions, AccessRequired::Unsatisfiable),
        LinkChange::Reset => set_password(actions),
        LinkChange::Approval(on) => set_access(actions, if on { AccessRequired::Administrator } else { AccessRequired::Any }),
    }
}

/// Desktop's `buildDeletePendingAdminApprovalMemberChange`: turns down a
/// request to join (banning them from asking again), or, for us, takes
/// ours back (`cancelJoinRequest`).
pub fn deny_request(
    secret: &GroupSecretParams,
    group: Option<&Group>,
    me: Aci,
    target: Aci,
    limit: usize,
    actions: &mut Actions,
) -> anyhow::Result<()> {
    if let Some(group) = group
        && !is_requesting(group, target)
    {
        bail!("they aren't asking to join");
    }
    actions
        .delete_members_pending_admin_approval
        .push(action::DeleteMemberPendingAdminApprovalAction { deleted_user_id: encrypt_id(secret, target.into()) });
    if let Some(group) = group {
        ban(secret, group, me, target.into(), limit, actions);
    }
    Ok(())
}

/// Desktop's `buildPromotePendingAdminApprovalMemberChange`: lets them in.
pub fn approve_request(secret: &GroupSecretParams, group: &Group, target: Aci, actions: &mut Actions) -> anyhow::Result<()> {
    if !is_requesting(group, target) {
        bail!("they aren't asking to join");
    }
    actions.promote_members_pending_admin_approval.push(action::PromoteMemberPendingAdminApprovalAction {
        user_id: encrypt_id(secret, target.into()),
        role: Role::Default.into(),
    });
    Ok(())
}

/// Desktop's `buildDeletePendingMemberChange`: withdraws invitations (or,
/// for our own, declines one). Those no longer invited are skipped.
pub fn revoke_invites(secret: &GroupSecretParams, group: &Group, targets: &[ServiceId], actions: &mut Actions) -> usize {
    let mut count = 0;
    for target in targets {
        if group.members_pending_profile_key.iter().any(|p| p.address == *target) {
            actions
                .delete_members_pending_profile_key
                .push(action::DeleteMemberPendingProfileKeyAction { deleted_user_id: encrypt_id(secret, *target) });
            count += 1;
        }
    }
    count
}

/// Which of our ids an invitation names, if one does: our ACI before our
/// PNI, as Desktop's `leaveGroupV2` checks.
pub fn our_invitation(group: &Group, aci: Aci, pni: Option<Pni>) -> Option<ServiceId> {
    let invited = |id: ServiceId| group.members_pending_profile_key.iter().any(|p| p.address == id);
    if invited(aci.into()) {
        return Some(aci.into());
    }
    pni.map(ServiceId::from).filter(|pni| invited(*pni))
}

/// Our profile key proof for the group server (`createProfileKeyCredentialPresentation`).
fn presentation(secret: &GroupSecretParams, params: &ServerPublicParams, credential: &ExpiringProfileKeyCredential) -> Vec<u8> {
    let randomness: [u8; 32] = rand::Rng::random(&mut rand::rng());
    zkgroup::serialize(&params.create_expiring_profile_key_credential_presentation(randomness, *secret, *credential))
}

/// Desktop's `buildPromoteMemberChange`: accepts an invitation, made to
/// our ACI or (`by_pni`) to our PNI.
pub fn accept_invite(
    secret: &GroupSecretParams,
    params: &ServerPublicParams,
    credential: &ExpiringProfileKeyCredential,
    by_pni: bool,
    actions: &mut Actions,
) {
    let presentation = presentation(secret, params, credential);
    if by_pni {
        actions.promote_members_pending_pni_aci_profile_key.push(action::PromoteMemberPendingPniAciProfileKeyAction {
            presentation,
            ..Default::default()
        });
    } else {
        actions
            .promote_members_pending_profile_key
            .push(action::PromoteMemberPendingProfileKeyAction { presentation, ..Default::default() });
    }
}

/// Joining by a group link: Desktop's `buildAddMember` (anyone may join)
/// or `buildAddPendingAdminApprovalMemberChange` (an admin approves).
pub fn join(
    secret: &GroupSecretParams,
    params: &ServerPublicParams,
    credential: &ExpiringProfileKeyCredential,
    approval: bool,
    actions: &mut Actions,
) {
    let presentation = presentation(secret, params, credential);
    if approval {
        actions.add_members_pending_admin_approval.push(action::AddMemberPendingAdminApprovalAction {
            added: Some(proto::MemberPendingAdminApproval { presentation, ..Default::default() }),
        });
    } else {
        actions.add_members.push(action::AddMemberAction {
            added: Some(proto::Member { presentation, role: Role::Default.into(), ..Default::default() }),
            join_from_invite_link: false,
        });
    }
}

/// A group photo, encrypted for the group's CDN (Desktop's `uploadAvatar`:
/// a `GroupAttributeBlob` with the image, padded as other group blobs).
pub fn encrypt_avatar(secret: &GroupSecretParams, image: &[u8]) -> Vec<u8> {
    let blob = proto::GroupAttributeBlob { content: Some(proto::group_attribute_blob::Content::Avatar(image.to_vec())) };
    let randomness: [u8; 32] = rand::Rng::random(&mut rand::rng());
    secret.encrypt_blob_with_padding(randomness, &blob.encode_to_vec(), 0)
}

/// A group string (a title or description, as a group link's join info
/// has them), decrypted (Desktop's `decryptGroupTitle` and
/// `decryptGroupDescription`).
pub fn decrypt_text(secret: &GroupSecretParams, bytes: &[u8]) -> Option<String> {
    use prost::Message as _;
    use proto::group_attribute_blob::Content;
    if bytes.is_empty() {
        return None;
    }
    let plain = secret.decrypt_blob_with_padding(bytes).ok()?;
    match proto::GroupAttributeBlob::decode(plain.as_slice()).ok()?.content? {
        Content::Title(text) | Content::DescriptionText(text) => Some(text),
        _ => None,
    }
}

/// `https://signal.group/#…` for a group's link (Desktop's
/// `buildGroupLink`): its master key and password in a `GroupInviteLink`,
/// URL-safe base64 without padding.
pub fn link_url(master_key: [u8; 32], password: &[u8]) -> String {
    use base64::Engine as _;
    use proto::group_invite_link::{Contents, GroupInviteLinkContentsV1};
    let link = proto::GroupInviteLink {
        contents: Some(Contents::ContentsV1(GroupInviteLinkContentsV1 {
            group_master_key: master_key.to_vec(),
            invite_link_password: password.to_vec(),
        })),
    };
    format!("https://signal.group/#{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(link.encode_to_vec()))
}

/// Why a group link can't be read, in Desktop's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// Not a group link at all, or a damaged one.
    Invalid,
    /// A newer link version than we know.
    UnknownVersion,
}

impl LinkError {
    /// Desktop's title and description (`icu:GroupV2--join--invalid-link`
    /// and `--unknown-link-version`).
    pub fn text(self) -> (&'static str, &'static str) {
        match self {
            LinkError::Invalid => (
                "Invalid Link",
                "This is not a valid group link. Make sure the entire link is intact and correct before attempting to join.",
            ),
            LinkError::UnknownVersion => ("Unknown link version", "This link is not supported by this version of Signal Desktop."),
        }
    }
}

/// Whether `url` is a group link, as Desktop's `groupInvitesRoute` matches
/// one: `https://signal.group/#…`, `sgnl://signal.group/#…` or
/// `sgnl://joingroup/#…`.
pub fn is_group_link(url: &str) -> bool {
    invite_code(url).is_some()
}

fn invite_code(url: &str) -> Option<&str> {
    let url = url.trim();
    let rest = ["https://signal.group", "sgnl://signal.group", "sgnl://joingroup"]
        .iter()
        .find_map(|prefix| url.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &url[prefix.len()..]))?;
    let code = rest.strip_prefix('/').unwrap_or(rest).strip_prefix('#')?;
    (!code.is_empty() && !code.contains('/')).then_some(code)
}

/// A group link's master key and password (Desktop's `parseGroupLink`).
pub fn parse_link(url: &str) -> Result<([u8; 32], Vec<u8>), LinkError> {
    use base64::Engine as _;
    use prost::Message as _;
    use proto::group_invite_link::Contents;
    let code = invite_code(url).ok_or(LinkError::Invalid)?;
    // Desktop's `fromWebSafeBase64`: padding or none.
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(code.trim_end_matches('='))
        .map_err(|_| LinkError::Invalid)?;
    let link = proto::GroupInviteLink::decode(bytes.as_slice()).map_err(|_| LinkError::Invalid)?;
    let Some(Contents::ContentsV1(contents)) = link.contents else { return Err(LinkError::UnknownVersion) };
    let key: [u8; 32] = contents.group_master_key.as_slice().try_into().map_err(|_| LinkError::Invalid)?;
    if contents.invite_link_password.is_empty() {
        return Err(LinkError::Invalid);
    }
    Ok((key, contents.invite_link_password))
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::{
        groups_v2::{AccessControl, BannedMember, GroupChange, GroupOperations, Member, PendingMember, RequestingMember},
        zkgroup::{ServerSecretParams, Timestamp, profiles::ProfileKey},
    };

    const KEY: [u8; 32] = [3; 32];
    const DAY: u64 = 86_400;

    fn aci(n: u8) -> Aci {
        Aci::from_uuid_bytes([n; 16])
    }

    fn pni(n: u8) -> Pni {
        Pni::from_uuid_bytes([n; 16])
    }

    fn member(n: u8, role: Role) -> Member {
        Member {
            aci: aci(n),
            role,
            profile_key: ProfileKey::create([n; 32]),
            joined_at_version: 0,
            label: None,
            label_emoji: None,
        }
    }

    /// Us (1, an admin), Bob (2) and Carol (3, an admin), with an
    /// invitation for 4, a request from 5, and 6 banned.
    fn group() -> Group {
        Group {
            title: "Hikers".into(),
            avatar: String::new(),
            disappearing_messages_timer: None,
            access_control: Some(AccessControl {
                attributes: AccessRequired::Member,
                members: AccessRequired::Member,
                add_from_invite_link: AccessRequired::Unsatisfiable,
                member_label: AccessRequired::Member,
            }),
            version: 7,
            members: vec![member(1, Role::Administrator), member(2, Role::Default), member(3, Role::Administrator)],
            members_pending_profile_key: vec![PendingMember {
                address: aci(4).into(),
                role: Role::Default,
                added_by_aci: aci(2),
                timestamp: 1,
            }],
            members_pending_admin_approval: vec![RequestingMember {
                aci: aci(5),
                profile_key: ProfileKey::create([5; 32]),
                timestamp: 2,
            }],
            invite_link_password: Vec::new(),
            description_text: None,
            announcements_only: false,
            members_banned: vec![BannedMember { user_id: aci(6).into(), timestamp: 3 }],
            terminated: false,
        }
    }

    /// The change as the server would hand it back (`sourceUserId` set,
    /// unsigned), decrypted the way every client reads one.
    fn decrypt(editor: Aci, actions: Actions) -> Vec<GroupChange> {
        let secret = secret_params(KEY);
        let actions = Actions {
            source_user_id: encrypt_id(&secret, editor.into()),
            group_id: secret.get_group_identifier().to_vec(),
            version: 8,
            ..actions
        };
        let change = proto::GroupChange { actions: actions.encode_to_vec(), server_signature: Vec::new(), change_epoch: 7 };
        let changes = GroupOperations::new(secret).decrypt_group_change(change).expect("decrypts");
        assert_eq!(changes.editor, editor);
        changes.changes
    }

    #[test]
    fn removing_a_member_bans_them_as_desktop_does() {
        let secret = secret_params(KEY);
        let mut actions = Actions::default();
        remove_member(&secret, &group(), aci(1), aci(2), 1000, &mut actions).unwrap();
        let changes = decrypt(aci(1), actions);
        assert!(matches!(changes[0], GroupChange::DeleteMember(a) if a == aci(2)));
        assert!(matches!(&changes[1], GroupChange::AddBannedMember(b) if b.user_id == ServiceId::from(aci(2))));
        assert_eq!(changes.len(), 2);
        // Someone not in the group isn't removed.
        assert!(remove_member(&secret, &group(), aci(1), aci(9), 1000, &mut Actions::default()).is_err());
    }

    #[test]
    fn bans_past_the_limit_drop_the_oldest() {
        let secret = secret_params(KEY);
        let mut full = group();
        full.members_banned.push(BannedMember { user_id: aci(7).into(), timestamp: 9 });
        let mut actions = Actions::default();
        // A limit of 2 keeps one old ban: the newest (7), so 6 goes.
        remove_member(&secret, &full, aci(1), aci(2), 2, &mut actions).unwrap();
        let changes = decrypt(aci(1), actions);
        assert!(changes.iter().any(|c| matches!(c, GroupChange::DeleteBannedMember(id) if *id == ServiceId::from(aci(6)))));
        assert!(!changes.iter().any(|c| matches!(c, GroupChange::DeleteBannedMember(id) if *id == ServiceId::from(aci(7)))));
        // Someone already banned isn't banned twice; we're never banned.
        let mut actions = Actions::default();
        ban(&secret, &full, aci(1), aci(6).into(), 1000, &mut actions);
        ban(&secret, &full, aci(1), aci(1).into(), 1000, &mut actions);
        assert_eq!(actions, Actions::default());
    }

    #[test]
    fn roles_change_and_admin_only_labels_go_with_the_role() {
        let secret = secret_params(KEY);
        let mut actions = Actions::default();
        set_role(&secret, &group(), aci(2), true, &mut actions).unwrap();
        let changes = decrypt(aci(1), actions);
        assert!(matches!(changes[..], [GroupChange::ModifyMemberRole { aci: a, role: Role::Administrator }] if a == aci(2)));

        let mut labelled = group();
        labelled.access_control.as_mut().unwrap().member_label = AccessRequired::Administrator;
        labelled.members[2].label = Some("Route planner".into());
        let mut actions = Actions::default();
        set_role(&secret, &labelled, aci(3), false, &mut actions).unwrap();
        let changes = decrypt(aci(1), actions);
        assert!(changes.iter().any(|c| matches!(c, GroupChange::ModifyMemberRole { role: Role::Default, .. })));
        assert!(changes.iter().any(|c| matches!(c, GroupChange::MemberLabel { label_string: None, .. })));
    }

    #[test]
    fn permissions_are_desktops_access_levels() {
        let mut actions = Actions::default();
        set_permission(Permission::EditInfo { admins_only: true }, &mut actions);
        set_permission(Permission::AddMembers { admins_only: false }, &mut actions);
        set_permission(Permission::SendMessages { admins_only: true }, &mut actions);
        let changes = decrypt(aci(1), actions);
        assert!(changes.iter().any(|c| matches!(c, GroupChange::AttributeAccess(AccessRequired::Administrator))));
        assert!(changes.iter().any(|c| matches!(c, GroupChange::MemberAccess(AccessRequired::Member))));
        assert!(changes.iter().any(|c| matches!(c, GroupChange::AnnouncementOnly(true))));
    }

    #[test]
    fn links_turn_on_with_a_password_once_and_reset_with_a_new_one() {
        use base64::Engine as _;
        let password = [9u8; LINK_PASSWORD_LENGTH];
        let encoded = base64::engine::general_purpose::STANDARD.encode(password);
        let mut actions = Actions::default();
        change_link(&group(), LinkChange::On, password, &mut actions);
        let changes = decrypt(aci(1), actions);
        assert!(changes.iter().any(|c| matches!(c, GroupChange::InviteLinkAccess(AccessRequired::Any))));
        assert!(changes.iter().any(|c| matches!(c, GroupChange::InviteLinkPassword(p) if *p == encoded)));
        // A group that had one keeps its password when turned on again.
        let mut had = group();
        had.invite_link_password = vec![1; LINK_PASSWORD_LENGTH];
        let mut actions = Actions::default();
        change_link(&had, LinkChange::On, password, &mut actions);
        assert!(actions.modify_invite_link_password.is_none());
        let mut actions = Actions::default();
        change_link(&had, LinkChange::Approval(true), password, &mut actions);
        change_link(&had, LinkChange::Reset, password, &mut actions);
        let changes = decrypt(aci(1), actions);
        assert!(changes.iter().any(|c| matches!(c, GroupChange::InviteLinkAccess(AccessRequired::Administrator))));
        assert!(changes.iter().any(|c| matches!(c, GroupChange::InviteLinkPassword(_))));
        let mut actions = Actions::default();
        change_link(&had, LinkChange::Off, password, &mut actions);
        assert!(matches!(decrypt(aci(1), actions)[..], [GroupChange::InviteLinkAccess(AccessRequired::Unsatisfiable)]));
    }

    #[test]
    fn requests_are_approved_or_turned_down_and_banned() {
        let secret = secret_params(KEY);
        let mut actions = Actions::default();
        approve_request(&secret, &group(), aci(5), &mut actions).unwrap();
        assert!(matches!(decrypt(aci(1), actions)[..], [GroupChange::PromoteRequestingMember { aci: a, role: Role::Default }] if a == aci(5)));
        let mut actions = Actions::default();
        deny_request(&secret, Some(&group()), aci(1), aci(5), 1000, &mut actions).unwrap();
        let changes = decrypt(aci(1), actions);
        assert!(changes.iter().any(|c| matches!(c, GroupChange::DeleteRequestingMember(a) if *a == aci(5))));
        assert!(changes.iter().any(|c| matches!(c, GroupChange::AddBannedMember(b) if b.user_id == ServiceId::from(aci(5)))));
        assert!(approve_request(&secret, &group(), aci(2), &mut Actions::default()).is_err());
        // Taking back our own request: we can't read the group, and aren't banned.
        let mut actions = Actions::default();
        deny_request(&secret, None, aci(5), aci(5), 1000, &mut actions).unwrap();
        assert!(matches!(decrypt(aci(5), actions)[..], [GroupChange::DeleteRequestingMember(a)] if a == aci(5)));
    }

    #[test]
    fn invitations_are_revoked_and_ours_found_by_aci_or_pni() {
        let secret = secret_params(KEY);
        let mut actions = Actions::default();
        assert_eq!(revoke_invites(&secret, &group(), &[aci(4).into(), aci(2).into()], &mut actions), 1);
        assert!(matches!(decrypt(aci(1), actions)[..], [GroupChange::DeletePendingMember(id)] if id == ServiceId::from(aci(4))));
        let mut by_pni = group();
        by_pni.members_pending_profile_key[0].address = pni(8).into();
        assert_eq!(our_invitation(&by_pni, aci(8), Some(pni(8))), Some(pni(8).into()));
        assert_eq!(our_invitation(&group(), aci(4), Some(pni(8))), Some(aci(4).into()));
        assert_eq!(our_invitation(&group(), aci(2), None), None);
    }

    /// A made-up group server's keys, and our expiring profile key
    /// credential from it, as `GroupSession::profile_key_credential` gets one.
    fn credential(me: Aci, key: ProfileKey) -> (ServerPublicParams, ExpiringProfileKeyCredential) {
        let server = ServerSecretParams::generate([4; 32]);
        let params = server.get_public_params();
        let now = 20_000 * DAY + 600;
        let context = params.create_profile_key_credential_request_context([5; 32], me, key);
        let response = server
            .issue_expiring_profile_key_credential(
                [6; 32],
                &context.get_request(),
                me,
                key.get_commitment(me),
                Timestamp::from_epoch_seconds(20_002 * DAY),
            )
            .unwrap();
        let credential =
            params.receive_expiring_profile_key_credential(&context, &response, Timestamp::from_epoch_seconds(now)).unwrap();
        (params, credential)
    }

    #[test]
    fn joining_accepting_and_asking_carry_our_profile_key_proof() {
        let secret = secret_params(KEY);
        let key = ProfileKey::create([8; 32]);
        let (params, credential) = credential(aci(8), key);
        let joined = |change: &GroupChange| match change {
            GroupChange::NewMember(m) => m.aci == aci(8) && m.profile_key.get_bytes() == key.get_bytes() && m.role == Role::Default,
            GroupChange::NewRequestingMember(r) => r.aci == aci(8) && r.profile_key.get_bytes() == key.get_bytes(),
            GroupChange::PromotePendingMember { address, profile_key } => {
                *address == ServiceId::from(aci(8)) && profile_key.get_bytes() == key.get_bytes()
            }
            _ => false,
        };
        for approval in [false, true] {
            let mut actions = Actions::default();
            join(&secret, &params, &credential, approval, &mut actions);
            let changes = decrypt(aci(8), actions);
            assert!(changes.len() == 1 && joined(&changes[0]), "{changes:?}");
            assert_eq!(matches!(changes[0], GroupChange::NewRequestingMember(_)), approval);
        }
        let mut actions = Actions::default();
        accept_invite(&secret, &params, &credential, false, &mut actions);
        let changes = decrypt(aci(8), actions);
        assert!(changes.len() == 1 && joined(&changes[0]), "{changes:?}");
        let mut actions = Actions::default();
        accept_invite(&secret, &params, &credential, true, &mut actions);
        assert_eq!(actions.promote_members_pending_pni_aci_profile_key.len(), 1);
    }

    #[test]
    fn join_info_titles_decrypt() {
        let secret = secret_params(KEY);
        let ops = GroupOperations::new(secret);
        assert_eq!(decrypt_text(&secret, &ops.encrypt_title("Hikers", &mut rand::rng())).as_deref(), Some("Hikers"));
        assert_eq!(decrypt_text(&secret, &ops.encrypt_description(Some("Bring water"), &mut rand::rng())).as_deref(), Some("Bring water"));
        assert_eq!(decrypt_text(&secret, &[]), None);
        assert_eq!(decrypt_text(&secret_params([4; 32]), &ops.encrypt_title("Hikers", &mut rand::rng())), None, "another group's key");
    }

    #[test]
    fn avatars_decrypt_as_group_photos() {
        let secret = secret_params(KEY);
        let photo = b"\xff\xd8\xff a made-up photo".to_vec();
        let encrypted = encrypt_avatar(&secret, &photo);
        assert_eq!(GroupOperations::new(secret).decrypt_avatar(&encrypted), Some(photo));
    }

    #[test]
    fn links_round_trip_in_desktops_forms() {
        let url = link_url(KEY, &[9; LINK_PASSWORD_LENGTH]);
        assert!(url.starts_with("https://signal.group/#") && !url.ends_with('='));
        assert_eq!(parse_link(&url), Ok((KEY, vec![9; LINK_PASSWORD_LENGTH])));
        let code = url.trim_start_matches("https://signal.group/#");
        for form in [format!("sgnl://signal.group/#{code}"), format!("sgnl://joingroup/#{code}"), format!("https://signal.group/#{code}==")] {
            assert!(is_group_link(&form), "{form}");
            assert_eq!(parse_link(&form).map(|(k, _)| k), Ok(KEY), "{form}");
        }
        assert!(!is_group_link("https://signal.group/"));
        assert!(!is_group_link("https://example.com/#abc"));
        assert_eq!(parse_link("https://signal.group/#!!!"), Err(LinkError::Invalid));
        // A link with only a field we don't know is a newer version.
        use base64::Engine as _;
        let newer = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0x10, 0x01]);
        assert_eq!(parse_link(&format!("https://signal.group/#{newer}")), Err(LinkError::UnknownVersion));
    }
}
