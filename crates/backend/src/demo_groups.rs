//! Group administration in the demo: each command changes the made-up
//! groups in the database and leaves the notice Desktop would, without
//! going anywhere. Nothing here is a real person or group.

use std::path::Path;

use signal_store::{
    AppDb, ConversationId, ConversationKind, ConversationUpdate, GroupInvite, GroupLink, GroupNotice, GroupRequest, GroupState,
    Inviter, Membership, Notice,
};

use crate::group_admin::{LinkChange, Permission};
use crate::{Command, Event};

/// Our made-up ACI (the demo's `SELF`).
const ME: &str = "00000000-0000-4000-8000-000000000001";
/// The made-up group a demo link leads to.
pub(crate) const LINKED_TITLE: &str = "Community garden 🌱";

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn key_of(conversation: &ConversationId) -> Option<[u8; 32]> {
    conversation.as_group()?.try_into().ok()
}

/// A new made-up link for a group.
fn new_link(conversation: &ConversationId, approval: bool) -> Option<GroupLink> {
    let password: [u8; crate::group_admin::LINK_PASSWORD_LENGTH] = rand::Rng::random(&mut rand::rng());
    Some(GroupLink { url: crate::group_admin::link_url(key_of(conversation)?, &password), approval })
}

/// Seeds the demo's group administration: the house group (ours to run)
/// has its link on with approval, a request from Priya and an invitation
/// for Sam; and Maya invited us to a cleanup crew.
pub(crate) async fn seed(db: &AppDb, house: &str, invited: &str, maya: &str, jordan: &str, priya: &str, sam: &str) -> signal_store::Result<()> {
    let at = now();
    let house = ConversationId(house.into());
    let state = GroupState {
        link: new_link(&house, true),
        invites: vec![GroupInvite { service_id: sam.into(), by: ME.into() }],
        requests: vec![GroupRequest { aci: priya.into(), at: at - 60 * 60_000 }],
        ..Default::default()
    };
    db.upsert_conversation(&house, ConversationUpdate { group_state: Some(state), ..Default::default() }).await?;
    let invited = ConversationId(invited.into());
    db.upsert_conversation(
        &invited,
        ConversationUpdate {
            kind: Some(ConversationKind::Group),
            title: Some("Trail cleanup crew 🧹".into()),
            members: Some(vec![maya.into(), jordan.into()]),
            admins: Some(vec![maya.into()]),
            description: Some("Picking up litter on the ridge trail, first Sunday of the month.".into()),
            group_state: Some(GroupState { membership: Membership::Invited { by: Some(maya.into()) }, ..Default::default() }),
            ..Default::default()
        },
    )
    .await?;
    let notice = Notice::Group { by: Some(maya.into()), change: GroupNotice::Invited { member: None } };
    db.insert_notice(&invited, at - 3 * 60 * 60_000, &notice).await?;
    Ok(())
}

/// Handles a group administration command; `false` when it isn't one.
pub(crate) async fn handle(db: &AppDb, data_dir: &Path, command: &Command, emit: &dyn Fn(Event)) -> bool {
    let notice = |conversation: ConversationId, change: GroupNotice| async move {
        let notice = Notice::Group { by: None, change };
        if let Ok(Some(id)) = db.insert_notice(&conversation, now(), &notice).await {
            emit(Event::MessageChanged { conversation, message_id: id });
        }
    };
    let chat = |conversation: ConversationId| async move { db.conversation(&conversation).await.ok().flatten() };
    let save = |conversation: ConversationId, update: ConversationUpdate| async move {
        let _ = db.upsert_conversation(&conversation, update).await;
    };
    match command.clone() {
        Command::RemoveFromGroup { conversation, service_id } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut state = group.group_state.clone();
            if let Some(ix) = state.requests.iter().position(|r| r.aci == service_id) {
                state.requests.remove(ix);
                state.banned.push(service_id.clone());
                save(conversation.clone(), ConversationUpdate { group_state: Some(state), ..Default::default() }).await;
                notice(conversation, GroupNotice::RequestRemoved { member: Some(service_id) }).await;
            } else if let Some(ix) = state.invites.iter().position(|i| i.service_id == service_id) {
                let invite = state.invites.remove(ix);
                let inviter = if invite.by == ME { Inviter::Us } else { Inviter::Person { id: invite.by } };
                save(conversation.clone(), ConversationUpdate { group_state: Some(state), ..Default::default() }).await;
                notice(conversation, GroupNotice::InviteRemoved { member: Some(service_id), inviter }).await;
            } else if group.members.contains(&service_id) {
                state.banned.push(service_id.clone());
                let members = group.members.iter().filter(|m| **m != service_id).cloned().collect();
                let admins = group.admins.iter().filter(|m| **m != service_id).cloned().collect();
                let update = ConversationUpdate { members: Some(members), admins: Some(admins), group_state: Some(state), ..Default::default() };
                save(conversation.clone(), update).await;
                notice(conversation, GroupNotice::Removed { member: Some(service_id) }).await;
            }
        }
        Command::SetGroupAdmin { conversation, member, admin } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut admins: Vec<String> = group.admins.iter().filter(|a| **a != member).cloned().collect();
            if admin {
                admins.push(member.clone());
            }
            save(conversation.clone(), ConversationUpdate { admins: Some(admins), ..Default::default() }).await;
            notice(conversation, GroupNotice::Admin { member: Some(member), admin }).await;
        }
        Command::SetGroupPermission { conversation, permission } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut state = group.group_state.clone();
            let change = match permission {
                Permission::EditInfo { admins_only } => {
                    state.info_admins_only = admins_only;
                    GroupNotice::InfoAccess { admins_only }
                }
                Permission::AddMembers { admins_only } => {
                    state.members_admins_only = admins_only;
                    GroupNotice::MemberAccess { admins_only }
                }
                Permission::SendMessages { admins_only } => {
                    state.announcements_only = admins_only;
                    GroupNotice::AnnouncementsOnly { on: admins_only }
                }
            };
            save(conversation.clone(), ConversationUpdate { group_state: Some(state), ..Default::default() }).await;
            notice(conversation, change).await;
        }
        Command::ChangeGroupLink { conversation, change } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut state = group.group_state.clone();
            let change = match change {
                LinkChange::On => {
                    state.link = new_link(&conversation, false);
                    GroupNotice::LinkOn { approval: false }
                }
                LinkChange::Off => {
                    state.link = None;
                    GroupNotice::LinkOff
                }
                LinkChange::Reset => {
                    let approval = state.link.as_ref().is_some_and(|l| l.approval);
                    state.link = new_link(&conversation, approval);
                    GroupNotice::LinkReset
                }
                LinkChange::Approval(on) => {
                    if let Some(link) = &mut state.link {
                        link.approval = on;
                    }
                    GroupNotice::LinkApproval { on }
                }
            };
            save(conversation.clone(), ConversationUpdate { group_state: Some(state), ..Default::default() }).await;
            notice(conversation, change).await;
        }
        Command::AnswerJoinRequest { conversation, member, approve } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut state = group.group_state.clone();
            state.requests.retain(|r| r.aci != member);
            let mut update = ConversationUpdate::default();
            let change = if approve {
                let mut members = group.members.clone();
                members.push(member.clone());
                update.members = Some(members);
                GroupNotice::Approved { member: Some(member) }
            } else {
                state.banned.push(member.clone());
                GroupNotice::RequestRemoved { member: Some(member) }
            };
            update.group_state = Some(state);
            save(conversation.clone(), update).await;
            notice(conversation, change).await;
        }
        Command::RevokeGroupInvites { conversation, invitees } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut state = group.group_state.clone();
            let before = state.invites.len();
            state.invites.retain(|i| !invitees.contains(&i.service_id));
            let count = (before - state.invites.len()) as u32;
            save(conversation.clone(), ConversationUpdate { group_state: Some(state), ..Default::default() }).await;
            let change = match invitees.as_slice() {
                [one] if count == 1 => GroupNotice::InviteRemoved { member: Some(one.clone()), inviter: Inviter::Unknown },
                _ => GroupNotice::InvitesRemoved { count, inviter: Inviter::Unknown },
            };
            notice(conversation, change).await;
        }
        Command::CancelJoinRequest { conversation } => {
            let Some(group) = chat(conversation.clone()).await else { return true };
            let mut state = group.group_state.clone();
            state.membership = Membership::Member;
            save(conversation.clone(), ConversationUpdate { group_state: Some(state), ..Default::default() }).await;
            notice(conversation, GroupNotice::RequestRemoved { member: None }).await;
        }
        // A link to one of the demo's groups opens it; any other good link
        // leads to the made-up garden group (asking an admin when the
        // link's password starts odd, so both kinds can be tried).
        Command::OpenGroupLink { url } => match crate::group_admin::parse_link(&url) {
            Err(error) => {
                let (title, description) = error.text();
                emit(Event::GroupLinkFailed { title: title.into(), description: description.into() });
            }
            Ok((key, password)) => {
                let conversation = ConversationId::group(&key);
                match chat(conversation.clone()).await {
                    Some(group) if !group.left && group.group_state.membership == Membership::Member => {
                        emit(Event::GroupOpened { conversation, toast: Some("You're already in this group.".into()) })
                    }
                    Some(group) if matches!(group.group_state.membership, Membership::Requesting { .. }) => emit(Event::GroupOpened {
                        conversation,
                        toast: Some("You have already requested approval to join this group.".into()),
                    }),
                    _ => emit(Event::GroupLinkPreview(crate::GroupLinkPreview {
                        url,
                        title: LINKED_TITLE.into(),
                        description: Some("Plots, tools and a shared compost bin.".into()),
                        member_count: 12,
                        approval: password.first().is_some_and(|b| b % 2 == 1),
                        avatar: None,
                    })),
                }
            }
        },
        Command::JoinGroupLink { url } => {
            let Ok((key, password)) = crate::group_admin::parse_link(&url) else { return true };
            let conversation = ConversationId::group(&key);
            let approval = password.first().is_some_and(|b| b % 2 == 1);
            let state = GroupState {
                membership: if approval {
                    use base64::Engine as _;
                    Membership::Requesting { password: base64::engine::general_purpose::STANDARD.encode(&password) }
                } else {
                    Membership::Member
                },
                ..Default::default()
            };
            let update = ConversationUpdate {
                kind: Some(ConversationKind::Group),
                title: Some(LINKED_TITLE.into()),
                members: Some(if approval { Vec::new() } else { vec![ME.into()] }),
                group_state: Some(state),
                ..Default::default()
            };
            save(conversation.clone(), update).await;
            let _ = db.set_left(&conversation, approval).await;
            let change = if approval { GroupNotice::Requested { member: None } } else { GroupNotice::JoinedByLink { member: None } };
            notice(conversation.clone(), change).await;
            emit(Event::ConversationsChanged);
            emit(Event::GroupOpened { conversation, toast: None });
            return true;
        }
        Command::SetGroupAvatar { conversation, image } => {
            let path = match image {
                Some(image) => {
                    let Ok(photo) = crate::media::group_avatar(&image) else {
                        emit(Event::Error("couldn't use that image".into()));
                        return true;
                    };
                    let dir = data_dir.join("avatars");
                    let file = dir.join(format!("demo-{}-{}.jpg", conversation.0.replace(':', "-"), now()));
                    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, photo)).is_err() {
                        return true;
                    }
                    Some(file)
                }
                None => None,
            };
            let _ = db.set_avatar(&conversation, path.as_deref().and_then(Path::to_str)).await;
            notice(conversation, GroupNotice::Avatar).await;
        }
        Command::RefreshGroup { .. } => return true,
        _ => return false,
    }
    emit(Event::ConversationsChanged);
    true
}

/// Takes our invitation (as `actor/group_admin.rs` would): a member now,
/// with Desktop's notice. Returns the notice to add.
pub(crate) async fn accept_invite(db: &AppDb, conversation: &ConversationId) -> Option<Notice> {
    let group = db.conversation(conversation).await.ok().flatten()?;
    let mut state = group.group_state.clone();
    let inviter = match &state.membership {
        Membership::Invited { by: Some(by) } => Inviter::Person { id: by.clone() },
        _ => Inviter::Unknown,
    };
    state.membership = Membership::Member;
    let mut members = group.members.clone();
    members.push(ME.into());
    let update = ConversationUpdate {
        members: Some(members),
        group_state: Some(state),
        can_label: Some(true),
        can_add_members: Some(true),
        ..Default::default()
    };
    let _ = db.upsert_conversation(conversation, update).await;
    Some(Notice::Group { by: None, change: GroupNotice::AcceptedInvite { member: None, inviter } })
}

/// Declines our invitation: out of the group, with Desktop's notice.
pub(crate) async fn decline_invite(db: &AppDb, conversation: &ConversationId) -> Notice {
    let _ = db.set_left(conversation, true).await;
    let update = ConversationUpdate { group_state: Some(GroupState::default()), ..Default::default() };
    let _ = db.upsert_conversation(conversation, update).await;
    Notice::Group { by: None, change: GroupNotice::InviteRemoved { member: None, inviter: Inviter::Unknown } }
}
