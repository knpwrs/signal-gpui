//! Group administration as Desktop does it: removing people, admins,
//! permissions and the group link (`ts/models/conversations.preload.ts`:
//! `removeFromGroupV2`, `toggleAdmin`, `toggleGroupLink` and the rest),
//! invitations and requests to join, the group photo, and joining by a
//! link (`ts/groups/joinViaLink.preload.ts`). The changes themselves are
//! built in `crate::group_admin`.

use presage::libsignal_service::groups_v2::Group;
use presage::proto::group_change::Actions;
use signal_store::{GroupState, Membership};

use super::*;
use crate::group_admin::{self as admin, LinkChange, Permission};
use crate::groups::{GroupSession, JoinInfoError};

/// How long after one fetch a group's chat or details opening fetches it
/// again (Desktop throttles `fetchLatestGroupV2Data` likewise).
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

fn group_key(conversation: &ConversationId) -> anyhow::Result<[u8; 32]> {
    conversation.as_group().and_then(|k| k.try_into().ok()).ok_or_else(|| anyhow::anyhow!("not a group"))
}

impl Actor {
    fn self_pni(&self) -> Option<presage::libsignal_service::protocol::Pni> {
        Some(self.manager()?.registration_data().service_ids.pni())
    }

    fn me(&self) -> anyhow::Result<Aci> {
        Aci::parse_from_service_id_string(&self.self_aci()).ok_or_else(|| anyhow::anyhow!("no account"))
    }

    /// One change at a time per group, as Desktop queues them
    /// (`conversation.queueJob('modifyGroupV2')`): two at once would both
    /// name the same next revision, and one would fail.
    pub(super) async fn group_lock(&self, conversation: &ConversationId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.group_locks.borrow_mut().entry(conversation.clone()).or_default().clone();
        lock.lock_owned().await
    }

    pub(super) async fn stored_group_state(&self, conversation: &ConversationId) -> GroupState {
        self.db.conversation(conversation).await.ok().flatten().map(|c| c.group_state).unwrap_or_default()
    }

    /// Held while a group's stored state is read and written again, so an
    /// incoming change, our own and a fetch don't undo each other's.
    pub(super) async fn group_state_lock(&self, conversation: &ConversationId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.group_state_locks.borrow_mut().entry(conversation.clone()).or_default().clone();
        lock.lock_owned().await
    }

    /// Changes a group's stored state (under [`Self::group_state_lock`]).
    pub(super) async fn update_group_state(&self, conversation: &ConversationId, change: impl FnOnce(&mut GroupState)) {
        let _lock = self.group_state_lock(conversation).await;
        let before = self.stored_group_state(conversation).await;
        let mut state = before.clone();
        change(&mut state);
        if state == before {
            return;
        }
        let update = ConversationUpdate { group_state: Some(state), ..Default::default() };
        if let Err(error) = self.db.upsert_conversation(conversation, update).await {
            warn!(%error, "failed to save the group's state");
        }
    }

    /// The group as the server has it, written here: presage's copy and
    /// what presage doesn't keep.
    pub(super) async fn save_full_group(&self, key: [u8; 32], group: Group) {
        let conversation = ConversationId::group(&key);
        let pni = self.self_pni().map(|p| p.service_id_string());
        let me = self.self_aci();
        self.update_group_state(&conversation, |state| *state = mapping::full_group_state(&group, key, &me, pni.as_deref(), state))
            .await;
        if let Err(error) = self.store.save_group(key, group).await {
            warn!(%error, "failed to save the group");
        }
    }

    /// A change built from the group as the server has it now, sent with
    /// [`Self::submit_group_change`]; `extra` are others to tell (Desktop's
    /// `extraConversationsForSend`: whoever was removed). Errors are
    /// reported as `what`.
    async fn admin_change(
        self: &Rc<Self>,
        conversation: &ConversationId,
        what: &str,
        extra: Vec<String>,
        build: impl AsyncFnOnce(&GroupSession, &Group, Aci, &mut Actions) -> anyhow::Result<()>,
    ) -> bool {
        let result = async {
            let manager = self.manager().ok_or_else(|| anyhow::anyhow!("not connected"))?;
            let key = group_key(conversation)?;
            let me = self.me()?;
            let _lock = self.group_lock(conversation).await;
            let session = GroupSession::new(&manager, key).await?;
            let current = session.fetch().await?;
            if current.terminated {
                anyhow::bail!("the group has ended");
            }
            let mut actions = Actions { version: current.version + 1, ..Default::default() };
            build(&session, &current, me, &mut actions).await?;
            self.submit_group_change(&session, conversation, key, &current, actions, extra).await
        }
        .await;
        match result {
            Ok(()) => true,
            Err(error) => {
                self.error(what, format!("{error:#}"));
                false
            }
        }
    }

    /// Desktop's `removeFromGroupV2`: a request to join is turned down, an
    /// invitation revoked, a member removed (the last two banned from
    /// rejoining by the link). They're told too.
    pub(super) async fn remove_from_group(self: Rc<Self>, conversation: ConversationId, service_id: String) {
        let Some(target) = ServiceId::parse_from_service_id_string(&service_id) else { return };
        let limit = crate::remote_config().group_size_hard_limit();
        self.admin_change(&conversation, "couldn't remove them", vec![service_id], async |session, group, me, actions| {
            let aci = match target {
                ServiceId::Aci(aci) => Some(aci),
                ServiceId::Pni(_) => None,
            };
            if let Some(aci) = aci.filter(|a| group.members_pending_admin_approval.iter().any(|r| r.aci == *a)) {
                admin::deny_request(&session.secret, Some(group), me, aci, limit, actions)
            } else if group.members_pending_profile_key.iter().any(|p| p.address == target) {
                admin::revoke_invites(&session.secret, group, &[target], actions);
                Ok(())
            } else if let Some(aci) = aci {
                admin::remove_member(&session.secret, group, me, aci, limit, actions)
            } else {
                anyhow::bail!("they're not in the group")
            }
        })
        .await;
    }

    pub(super) async fn set_group_admin(self: Rc<Self>, conversation: ConversationId, member: String, make_admin: bool) {
        let Some(target) = Aci::parse_from_service_id_string(&member) else { return };
        self.admin_change(&conversation, "couldn't change their admin role", Vec::new(), async |session, group, _, actions| {
            admin::set_role(&session.secret, group, target, make_admin, actions)
        })
        .await;
    }

    pub(super) async fn set_group_permission(self: Rc<Self>, conversation: ConversationId, permission: Permission) {
        self.admin_change(&conversation, "couldn't change the group's permissions", Vec::new(), async |_, _, _, actions| {
            admin::set_permission(permission, actions);
            Ok(())
        })
        .await;
    }

    pub(super) async fn change_group_link(self: Rc<Self>, conversation: ConversationId, change: LinkChange) {
        let password: [u8; admin::LINK_PASSWORD_LENGTH] = rand::Rng::random(&mut rand::rng());
        self.admin_change(&conversation, "couldn't change the group link", Vec::new(), async |_, group, _, actions| {
            admin::change_link(group, change, password, actions);
            Ok(())
        })
        .await;
    }

    /// Desktop's `approvePendingApprovalRequest` and
    /// `denyPendingApprovalRequest` (who's turned down is told).
    pub(super) async fn answer_join_request(self: Rc<Self>, conversation: ConversationId, member: String, approve: bool) {
        let Some(target) = Aci::parse_from_service_id_string(&member) else { return };
        let limit = crate::remote_config().group_size_hard_limit();
        let extra = if approve { Vec::new() } else { vec![member] };
        self.admin_change(&conversation, "couldn't answer the request", extra, async |session, group, me, actions| {
            if approve {
                admin::approve_request(&session.secret, group, target, actions)
            } else {
                admin::deny_request(&session.secret, Some(group), me, target, limit, actions)
            }
        })
        .await;
    }

    pub(super) async fn revoke_group_invites(self: Rc<Self>, conversation: ConversationId, invitees: Vec<String>) {
        let targets: Vec<ServiceId> = invitees.iter().filter_map(|s| ServiceId::parse_from_service_id_string(s)).collect();
        self.admin_change(&conversation, "couldn't revoke the invitation", invitees, async |session, group, _, actions| {
            if admin::revoke_invites(&session.secret, group, &targets, actions) == 0 {
                anyhow::bail!("they're no longer invited");
            }
            Ok(())
        })
        .await;
    }

    /// Desktop's "Accept" on an invitation (`promotePendingMember`): our
    /// profile key proof, for the ACI or PNI we were invited by. The message
    /// request bar's Accept calls this for an invitation.
    /// Whether it went (the message request bar accepts through this).
    pub(super) async fn accept_group_invite(self: Rc<Self>, conversation: ConversationId) -> bool {
        let Some(manager) = self.manager() else { return false };
        let profile_key = manager.registration_data().profile_key();
        let pni = self.self_pni();
        self.admin_change(&conversation, "couldn't accept the invitation", Vec::new(), async |session, group, me, actions| {
            let invited = admin::our_invitation(group, me, pni).ok_or_else(|| anyhow::anyhow!("you're no longer invited"))?;
            let credential = session.profile_key_credential(me, profile_key).await.context("getting our own credential")?;
            admin::accept_invite(&session.secret, &session.params, &credential, matches!(invited, ServiceId::Pni(_)), actions);
            Ok(())
        })
        .await
    }

    /// Declines our invitation (Desktop's `leaveGroupV2` for a pending
    /// member); whether it went.
    pub(super) async fn decline_invite(self: &Rc<Self>, conversation: &ConversationId) -> bool {
        let pni = self.self_pni();
        self.admin_change(conversation, "couldn't decline the invitation", Vec::new(), async |session, group, me, actions| {
            let invited = admin::our_invitation(group, me, pni).ok_or_else(|| anyhow::anyhow!("you're no longer invited"))?;
            admin::revoke_invites(&session.secret, group, &[invited], actions);
            Ok(())
        })
        .await
    }

    /// Desktop's "Cancel Request" (`cancelJoinRequest`): we can't read the
    /// group, so the link's join info gives its revision, and the change
    /// goes with the link's password.
    pub(super) async fn cancel_join_request(self: Rc<Self>, conversation: ConversationId) {
        let result = async {
            let manager = self.manager().ok_or_else(|| anyhow::anyhow!("not connected"))?;
            let key = group_key(&conversation)?;
            let me = self.me()?;
            let Membership::Requesting { password } = self.stored_group_state(&conversation).await.membership else {
                anyhow::bail!("you haven't asked to join");
            };
            use base64::Engine as _;
            let password = base64::engine::general_purpose::STANDARD.decode(password).context("no link password")?;
            let _lock = self.group_lock(&conversation).await;
            let session = GroupSession::new(&manager, key).await?;
            let info = session.join_info(&password).await.map_err(|e| anyhow::anyhow!("{}", e.text().1))?;
            let mut actions = Actions { version: info.version + 1, ..Default::default() };
            admin::deny_request(&session.secret, None, me, me, 0, &mut actions)?;
            let signed = session.modify_with_link(actions, Some(&password)).await?;
            self.apply_group_change(key, &signed, self.next_timestamp()).await;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            self.error("couldn't cancel your request", format!("{error:#}"));
        }
    }

    /// The group's photo from a picked image (Desktop's
    /// `processImageFile`), or none.
    pub(super) async fn set_group_avatar(self: Rc<Self>, conversation: ConversationId, image: Option<Vec<u8>>) {
        let photo = match image {
            Some(image) => match tokio::task::spawn_blocking(move || crate::media::group_avatar(&image)).await {
                Ok(Ok(photo)) => Some(photo),
                Ok(Err(error)) => return self.error("couldn't use that image", format!("{error:#}")),
                Err(error) => return self.error("couldn't use that image", error),
            },
            None => None,
        };
        self.admin_change(&conversation, "couldn't change the group photo", Vec::new(), async |session, _, _, actions| {
            let avatar = match &photo {
                Some(photo) => session.upload_avatar(photo).await?,
                None => String::new(),
            };
            actions.modify_avatar = Some(presage::proto::group_change::actions::ModifyAvatarAction { avatar });
            Ok(())
        })
        .await;
    }

    /// Desktop's `fetchLatestGroupV2Data`: the group as the server has it,
    /// at most every few minutes. One we can't read (we left, or only asked
    /// to join) is left as it is.
    pub(super) async fn refresh_group(self: Rc<Self>, conversation: ConversationId) {
        let Ok(key) = group_key(&conversation) else { return };
        let Some(manager) = self.manager() else { return };
        {
            let mut fetched = self.group_fetches.borrow_mut();
            if fetched.get(&conversation).is_some_and(|at| at.elapsed() < REFRESH_INTERVAL) {
                return;
            }
            fetched.insert(conversation.clone(), std::time::Instant::now());
        }
        let stored = self.db.conversation(&conversation).await.ok().flatten();
        if stored.as_ref().is_none_or(|c| c.left || matches!(c.group_state.membership, Membership::Requesting { .. })) {
            return;
        }
        let _lock = self.group_lock(&conversation).await;
        let group = match GroupSession::new(&manager, key).await {
            Ok(session) => session.fetch().await,
            Err(error) => Err(error),
        };
        match group {
            Ok(group) => {
                self.save_full_group(key, group).await;
                self.titled.borrow_mut().remove(&conversation);
                self.ensure_title(&conversation).await;
                self.emit(Event::ConversationsChanged);
            }
            Err(error) => debug!(error = %format!("{error:#}"), "couldn't fetch the group"),
        }
    }

    /// A `signal.group` link was opened (Desktop's `joinViaLink` up to its
    /// dialog): the group it leads to, unless we're in it already.
    pub(super) async fn open_group_link(self: Rc<Self>, url: String) {
        let failed = |(title, description): (&str, &str)| Event::GroupLinkFailed { title: title.into(), description: description.into() };
        let (key, password) = match admin::parse_link(&url) {
            Ok(parsed) => parsed,
            Err(error) => return self.emit(failed(error.text())),
        };
        let conversation = ConversationId::group(&key);
        let existing = self.db.conversation(&conversation).await.ok().flatten();
        let me = self.self_aci();
        if existing.as_ref().is_some_and(|c| !c.left && c.members.contains(&me)) {
            return self.emit(Event::GroupOpened { conversation, toast: Some("You're already in this group.".into()) });
        }
        let Some(manager) = self.manager() else { return self.emit(failed(JoinInfoError::Failed.text())) };
        let session = match GroupSession::new(&manager, key).await {
            Ok(session) => session,
            Err(_) => return self.emit(failed(JoinInfoError::Failed.text())),
        };
        let info = match session.join_info(&password).await {
            Ok(info) => info,
            Err(error) => return self.emit(failed(error.text())),
        };
        let access = presage::libsignal_service::groups_v2::AccessRequired::try_from(info.add_from_invite_link)
            .unwrap_or(presage::libsignal_service::groups_v2::AccessRequired::Unknown);
        if !admin::link_enabled(access) {
            return self.emit(failed(JoinInfoError::Revoked.text()));
        }
        let approval = access == presage::libsignal_service::groups_v2::AccessRequired::Administrator;
        if approval && existing.as_ref().is_some_and(|c| matches!(c.group_state.membership, Membership::Requesting { .. })) {
            return self.emit(Event::GroupOpened {
                conversation,
                toast: Some("You have already requested approval to join this group.".into()),
            });
        }
        let avatar = match info.avatar.as_str() {
            "" => None,
            path => self.save_link_avatar(&manager, key, path).await,
        };
        self.emit(Event::GroupLinkPreview(crate::GroupLinkPreview {
            url,
            title: admin::decrypt_text(&session.secret, &info.title).filter(|t| !t.is_empty()).unwrap_or_else(|| "Unknown group".into()),
            description: admin::decrypt_text(&session.secret, &info.description).filter(|d| !d.is_empty()),
            member_count: info.member_count.max(1),
            approval,
            avatar,
        }));
    }

    /// A linked group's photo, for the join dialog (and kept as the chat's
    /// photo if we join): named after its path, as other group photos.
    async fn save_link_avatar(&self, manager: &SignalManager, key: [u8; 32], path: &str) -> Option<std::path::PathBuf> {
        let name = format!("link-{}", &hex::encode(<sha2::Sha256 as sha2::Digest>::digest(path.as_bytes()))[..16]);
        let file = self.config.data_dir.join("avatars").join(format!("{name}.jpg"));
        if file.exists() {
            return Some(file);
        }
        match crate::groups::download_avatar(manager, key, path).await {
            Ok(Some(bytes)) => {
                // `sync_avatars` may not have made the folder yet.
                std::fs::create_dir_all(file.parent()?).ok()?;
                std::fs::write(&file, bytes).ok().map(|_| file)
            }
            Ok(None) => None,
            Err(error) => {
                debug!(error = %format!("{error:#}"), "couldn't download the linked group's photo");
                None
            }
        }
    }

    /// Desktop's `joinGroupV2ViaLink`: joins, or asks an admin to let us
    /// in, then opens the chat.
    pub(super) async fn join_group_link(self: Rc<Self>, url: String) {
        let failed = |(title, description): (&str, &str)| Event::GroupLinkFailed { title: title.into(), description: description.into() };
        let (key, password) = match admin::parse_link(&url) {
            Ok(parsed) => parsed,
            Err(error) => return self.emit(failed(error.text())),
        };
        let conversation = ConversationId::group(&key);
        match self.join_by_link(key, &password).await {
            Ok(()) => self.emit(Event::GroupOpened { conversation, toast: None }),
            Err(error) => {
                warn!(error = %format!("{error:#}"), "couldn't join by the group link");
                let reason = error.downcast_ref::<JoinInfoError>().copied().unwrap_or(JoinInfoError::Failed);
                self.emit(failed(reason.text()));
            }
        }
    }

    async fn join_by_link(self: &Rc<Self>, key: [u8; 32], password: &[u8]) -> anyhow::Result<()> {
        use base64::Engine as _;
        use presage::libsignal_service::groups_v2::AccessRequired;
        let manager = self.manager().ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let me = self.me()?;
        let conversation = ConversationId::group(&key);
        let _lock = self.group_lock(&conversation).await;
        let session = GroupSession::new(&manager, key).await?;
        let info = session.join_info(password).await?;
        let access = AccessRequired::try_from(info.add_from_invite_link).unwrap_or(AccessRequired::Unknown);
        if !admin::link_enabled(access) {
            return Err(JoinInfoError::Revoked.into());
        }
        let approval = access == AccessRequired::Administrator;
        let credential = session.profile_key_credential(me, manager.registration_data().profile_key()).await?;
        let mut actions = Actions { version: info.version + 1, ..Default::default() };
        admin::join(&session.secret, &session.params, &credential, approval, &mut actions);
        let signed = match session.modify_with_link(actions, Some(password)).await {
            Ok(signed) => Some(signed),
            // Desktop: the server says we've asked already; we have.
            Err(error) if approval && error.downcast_ref::<crate::groups::AlreadyRequested>().is_some() => None,
            Err(error) => return Err(error),
        };
        // The chat, once the change went (Desktop drops its temporary
        // conversation when joining fails): its name and photo from the link.
        let title = admin::decrypt_text(&session.secret, &info.title).filter(|t| !t.is_empty());
        let description = admin::decrypt_text(&session.secret, &info.description);
        let update = ConversationUpdate {
            kind: Some(ConversationKind::Group),
            title,
            description: Some(description.unwrap_or_default()),
            color: Some(signal_store::avatar_color_of_bytes(&session.secret.get_group_identifier())),
            ..Default::default()
        };
        self.db.upsert_conversation(&conversation, update).await?;
        if !info.avatar.is_empty()
            && let Some(file) = self.save_link_avatar(&manager, key, &info.avatar).await
        {
            let _ = self.db.set_avatar(&conversation, file.to_str()).await;
        }
        if approval {
            let password = base64::engine::general_purpose::STANDARD.encode(password);
            self.update_group_state(&conversation, |state| state.membership = Membership::Requesting { password }).await;
            let _ = self.db.set_left(&conversation, true).await;
            if let Some(signed) = signed {
                self.apply_group_change(key, &signed, self.next_timestamp()).await;
            }
            self.emit(Event::ConversationsChanged);
            return Ok(());
        }
        let signed = signed.context("no change to apply")?;
        let timestamp = self.next_timestamp();
        self.apply_group_change(key, &signed, timestamp).await;
        let group = session.fetch().await?;
        let revision = group.version;
        self.save_full_group(key, group).await;
        let _ = self.db.set_left(&conversation, false).await;
        self.titled.borrow_mut().remove(&conversation);
        self.ensure_title(&conversation).await;
        self.emit(Event::ConversationsChanged);
        let refresh = self.clone();
        tokio::task::spawn_local(async move {
            if let Some(manager) = refresh.manager()
                && refresh.refresh_group_avatar(&manager, key, false).await
            {
                refresh.emit(Event::ConversationsChanged);
            }
        });
        // The members hear of it from us, as of any change.
        let data = DataMessage {
            group_v2: Some(GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(revision), group_change: Some(signed) }),
            timestamp: Some(timestamp as u64),
            ..Default::default()
        };
        self.enqueue(Outbound::Send { conversation, body: data.into(), timestamp: timestamp as u64, message: None });
        Ok(())
    }
}
