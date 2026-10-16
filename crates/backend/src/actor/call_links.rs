//! Call links, as Desktop has them (`ts/services/calling.preload.ts`,
//! `ts/state/ducks/calling.preload.ts`): making one, opening one's URL,
//! renaming it and changing its approval setting, deleting it, peeking and
//! joining its call (where an admin lets people in), its calls in the call
//! history, and keeping our other devices told (a `CallLinkUpdate` sync,
//! the Storage Service's `CallLinkRecord`, and `AD_HOC_CALL` call events).

use futures::channel::oneshot;
use rand::Rng as _;
use signal_calls::{LinkClient, LinkRestrictions, LinkResult, LinkState, Peek};
use signal_store::{AdhocCall, AdhocCallStatus, CallLink, CallLinkRestrictions};

use super::*;
use crate::call_links::{self as creds, AuthCredential};
use crate::{CallLinkCall, storage::StorageCallLink};

/// Desktop's `CallLinkNameMaxByteLength`.
pub(crate) const NAME_MAX_BYTES: usize = 120;

/// Links read from the calling server per refresh (Desktop throttles its
/// `callLinkRefreshJobQueue` the same way); the rest wait for the next.
const REFRESH_PER_RUN: usize = 10;

/// The call link part of a group call in progress.
#[derive(Debug, Clone)]
pub(crate) struct LinkCallState {
    pub room_id: String,
    pub admin: bool,
    /// Who's asking to join (ACIs).
    pub pending: Vec<String>,
    /// We're waiting to be let in.
    pub waiting: bool,
}

impl LinkCallState {
    pub(super) fn view(&self) -> CallLinkCall {
        CallLinkCall { admin: self.admin, pending: self.pending.clone(), waiting: self.waiting }
    }
}

fn restrictions_of(restrictions: LinkRestrictions) -> CallLinkRestrictions {
    match restrictions {
        LinkRestrictions::None => CallLinkRestrictions::None,
        LinkRestrictions::AdminApproval => CallLinkRestrictions::AdminApproval,
        LinkRestrictions::Unknown => CallLinkRestrictions::Unknown,
    }
}

fn restrictions_to(restrictions: CallLinkRestrictions) -> LinkRestrictions {
    match restrictions {
        CallLinkRestrictions::None => LinkRestrictions::None,
        CallLinkRestrictions::AdminApproval => LinkRestrictions::AdminApproval,
        CallLinkRestrictions::Unknown => LinkRestrictions::Unknown,
    }
}

/// Cuts a name to Desktop's limit, on a character boundary.
pub(crate) fn clamp_name(name: &str) -> String {
    let name = name.trim();
    let mut end = name.len().min(NAME_MAX_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// What a failed call link request means to people.
fn link_error(code: u16) -> String {
    match code {
        404 | 703 | 704 => "This call link is no longer valid".into(),
        602 => "Couldn't reach Signal's calling server".into(),
        code => format!("the calling server answered {code}"),
    }
}

/// Waits for a [`LinkClient`] answer.
async fn answer<T>(start: impl FnOnce(Box<dyn FnOnce(LinkResult<T>) + Send>)) -> LinkResult<T>
where
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    start(Box::new(move |result| {
        let _ = tx.send(result);
    }));
    rx.await.unwrap_or(Err(602))
}

impl Actor {
    /// The calling server's call link API, its requests made here.
    fn link_client(&self) -> LinkClient {
        self.link_client
            .get_or_init(|| {
                let runtime = tokio::runtime::Handle::current();
                LinkClient::new(move |request_id, request, responder| {
                    runtime.spawn(async move {
                        let method = super::group_calls::method_of(request.method);
                        let response = super::group_calls::calling_server_request(method, request).await;
                        responder.respond(request_id, response);
                    });
                })
            })
            .clone()
    }

    /// Today's call link credential presented for `root_key`, fetching
    /// the day's credential once (Desktop keeps a week's; one is enough
    /// for the requests of a day).
    async fn link_auth(&self, root_key: &[u8]) -> anyhow::Result<Vec<u8>> {
        let manager = self.manager().context("not connected")?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let today = crate::groups::today(now);
        let cached = self.link_auth.borrow().clone().filter(|c: &AuthCredential| c.day == today);
        let credential = match cached {
            Some(credential) => credential,
            None => {
                let credential = creds::fetch_auth_credential(&manager).await?;
                *self.link_auth.borrow_mut() = Some(credential.clone());
                credential
            }
        };
        creds::auth_presentation(&credential, manager.registration_data().service_ids.aci(), root_key)
    }

    /// Reads a link's state from the calling server.
    async fn read_link(&self, root_key: &[u8]) -> anyhow::Result<LinkResult<LinkState>> {
        let auth = self.link_auth(root_key).await?;
        let client = self.link_client();
        let key = root_key.to_vec();
        Ok(answer(move |done| client.read(&key, &auth, done)).await)
    }

    async fn save_link_state(&self, room_id: &str, state: &LinkState) -> bool {
        let restrictions = restrictions_of(state.restrictions);
        let name = clamp_name(&state.name);
        match self.db.set_call_link_state(room_id, &name, restrictions, state.revoked, Some(state.expiration)).await {
            Ok(changed) => changed,
            Err(error) => {
                self.error("failed to save the call link", error);
                false
            }
        }
    }

    /// Desktop's "Create a call link": a new link that asks for admin
    /// approval, kept here, in Storage Service and on our other devices,
    /// with a placeholder in the call history.
    pub(super) async fn create_call_link(&self) {
        let Some(manager) = self.manager() else { return self.error("can't create a call link", "not connected") };
        let (key, admin_key) = signal_calls::new_link_keys();
        let Some(room) = signal_calls::room_id(&key) else { return };
        let auth = match creds::create_auth(&manager, &room, &key).await {
            Ok(auth) => auth,
            Err(error) => return self.error("can't create a call link", format!("{error:#}")),
        };
        let client = self.link_client();
        let (k, a) = (key.clone(), admin_key.clone());
        let state = answer(move |done| {
            client.create(&k, &auth.presentation, &a, &auth.public_params, LinkRestrictions::AdminApproval, done)
        })
        .await;
        let state = match state {
            Ok(state) => state,
            Err(code) => return self.error("can't create a call link", link_error(code)),
        };
        let room_id = hex::encode(&room);
        let now = now_millis();
        let link = CallLink {
            room_id: room_id.clone(),
            // With its epoch, as the server gave it back.
            root_key: state.root_key.clone(),
            admin_key: Some(admin_key.clone()),
            name: clamp_name(&state.name),
            restrictions: restrictions_of(state.restrictions),
            revoked: state.revoked,
            expiration: Some(state.expiration),
            deleted_at: None,
            added_at: now,
        };
        if let Err(error) = self.db.add_call_link(&link).await {
            return self.error("failed to save the call link", error);
        }
        self.add_link_placeholder(&room_id, now).await;
        self.sync_call_link_update(&link.root_key, Some(&admin_key));
        self.spawn_write_call_link(link.root_key.clone(), Some(admin_key), 0);
        self.emit(Event::CallLinksChanged);
        self.emit(Event::CallLinkCreated(room_id));
    }

    /// Desktop's `toCallHistoryFromUnusedCallLink`: a link of ours shows in
    /// the call history before anyone calls through it.
    async fn add_link_placeholder(&self, room_id: &str, at: i64) {
        let call = AdhocCall { room_id: room_id.to_owned(), call_id: rand::rng().random(), at, status: AdhocCallStatus::Pending };
        if let Err(error) = self.db.record_adhoc_call(&call).await {
            warn!(%error, "failed to add the call link to the call history");
        }
    }

    /// Renames a link of ours, or changes who may join.
    pub(super) async fn update_call_link(&self, room_id: String, name: Option<String>, restrictions: Option<CallLinkRestrictions>) {
        let link = match self.db.call_link(&room_id).await {
            Ok(Some(link)) if link.deleted_at.is_none() => link,
            Ok(_) => return,
            Err(error) => return self.error("failed to load the call link", error),
        };
        let Some(admin_key) = link.admin_key.clone().filter(|k| !k.is_empty()) else {
            return self.error("can't change the call link", "only its admin can");
        };
        let restrictions = restrictions.map(restrictions_to).filter(|r| *r != LinkRestrictions::Unknown);
        let name = name.map(|n| clamp_name(&n));
        if name.is_none() && restrictions.is_none() {
            return;
        }
        let auth = match self.link_auth(&link.root_key).await {
            Ok(auth) => auth,
            Err(error) => return self.error("can't change the call link", format!("{error:#}")),
        };
        let client = self.link_client();
        let key = link.root_key.clone();
        let state = answer(move |done| client.update(&key, &auth, &admin_key, name.as_deref(), restrictions, done)).await;
        match state {
            Ok(state) => {
                self.save_link_state(&room_id, &state).await;
                // Desktop tells our other devices, which read it again.
                self.sync_call_link_update(&link.root_key, link.admin_key.as_deref());
                self.emit(Event::CallLinksChanged);
            }
            Err(code) => self.error("can't change the call link", link_error(code)),
        }
    }

    /// Deletes a link of ours for everyone (Desktop's "Delete link"), or
    /// forgets one we only joined. Ours is marked deleted only once the
    /// calling server has let it go (or never had it).
    pub(super) async fn delete_call_link(&self, room_id: String) {
        let link = match self.db.call_link(&room_id).await {
            Ok(Some(link)) => link,
            Ok(None) => return,
            Err(error) => return self.error("failed to load the call link", error),
        };
        if self.link_call_room().as_deref() == Some(room_id.as_str()) {
            return self.error("can't delete the call link", "its call is in progress");
        }
        if !link.is_admin() {
            if let Err(error) = self.db.remove_call_link(&room_id).await {
                return self.error("failed to remove the call link", error);
            }
            return self.emit(Event::CallLinksChanged);
        }
        if self.delete_admin_link(&link).await {
            self.emit(Event::CallLinksChanged);
        }
    }

    /// Deletes a link of ours on the calling server, then here and in
    /// Storage Service. `false` (with the error shown): it's still there.
    async fn delete_admin_link(&self, link: &CallLink) -> bool {
        let Some(admin_key) = link.admin_key.clone().filter(|k| !k.is_empty()) else { return false };
        let auth = match self.link_auth(&link.root_key).await {
            Ok(auth) => auth,
            Err(error) => {
                self.error("can't delete the call link", format!("{error:#}"));
                return false;
            }
        };
        let client = self.link_client();
        let key = link.root_key.clone();
        match answer(move |done| client.delete(&key, &auth, &admin_key, done)).await {
            // Desktop: 404 means it's deleted already.
            Ok(()) | Err(404) => {}
            Err(code) => {
                self.error("can't delete the call link", link_error(code));
                return false;
            }
        }
        let now = now_millis();
        if let Err(error) = self.db.mark_call_link_deleted(&link.room_id, now).await {
            self.error("failed to delete the call link", error);
            return false;
        }
        self.spawn_write_call_link(link.root_key.clone(), None, now as u64);
        true
    }

    /// Keeps a link opened from its URL and reads its name (Desktop's
    /// `startCallLinkLobbyByRoomId` after `readCallLink`).
    pub(super) async fn open_call_link(&self, url: String) {
        let Some(key) = signal_calls::parse_link(&url) else {
            return self.error("can't open the call link", "it isn't a valid call link");
        };
        let Some(room) = signal_calls::room_id(&key) else { return };
        let room_id = hex::encode(room);
        let state = match self.read_link(&key).await {
            Ok(Ok(state)) => state,
            Ok(Err(code)) => return self.error("can't open the call link", link_error(code)),
            Err(error) => return self.error("can't open the call link", format!("{error:#}")),
        };
        let link = CallLink { room_id: room_id.clone(), root_key: key, added_at: now_millis(), ..Default::default() };
        match self.db.call_link(&room_id).await {
            Ok(Some(existing)) if existing.deleted_at.is_some() => {
                return self.error("can't open the call link", "you deleted this call link");
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.db.add_call_link(&link).await {
                    return self.error("failed to save the call link", error);
                }
            }
            Err(error) => return self.error("failed to load the call link", error),
        }
        self.save_link_state(&room_id, &state).await;
        self.emit(Event::CallLinksChanged);
        self.emit(Event::CallLinkOpened(room_id));
    }

    /// Who's in a link's call, for the lobby (Desktop's `peekCallLinkCall`),
    /// answered as a group call's peek is.
    pub(super) async fn peek_call_link(&self, conversation: ConversationId) {
        let Some(room_id) = conversation.as_call_link().map(str::to_owned) else { return };
        let Ok(Some(link)) = self.db.call_link(&room_id).await else { return };
        let auth = match self.link_auth(&link.root_key).await {
            Ok(auth) => auth,
            Err(error) => return warn!(error = %format!("{error:#}"), "couldn't peek the call link"),
        };
        let client = self.link_client();
        let key = link.root_key.clone();
        let peek = answer(move |done| client.peek(&key, &auth, done)).await;
        let users = match &peek {
            Ok(peek) => {
                self.on_link_call_peek(&room_id, peek).await;
                peek.users.iter().filter_map(|u| aci_of_bytes(u)).collect()
            }
            // No call yet is no one in it.
            Err(code) => {
                debug!(code, "call link peek");
                Vec::new()
            }
        };
        self.emit(Event::GroupPeek { conversation, users });
    }

    /// What a peek says about a link's call: the link's state, and a call
    /// going on (Desktop records one it sees, `Started`, as generic).
    pub(super) async fn on_link_call_peek(&self, room_id: &str, peek: &Peek) {
        let mut changed = false;
        if let Some(state) = &peek.link {
            changed |= self.save_link_state(room_id, state).await;
        }
        if let Some(era) = peek.era_id.as_deref().filter(|_| !peek.users.is_empty()) {
            changed |= self.record_adhoc(room_id, era, false).await;
        }
        if changed {
            self.emit(Event::CallLinksChanged);
        }
    }

    /// Records a link's call in the history; `joined`: we were in it,
    /// which our other devices hear of too (an `AD_HOC_CALL` call event).
    pub(super) async fn record_link_call(&self, room_id: &str, era: &str, joined: bool) {
        if self.record_adhoc(room_id, era, joined).await {
            self.emit(Event::CallLinksChanged);
        }
    }

    async fn record_adhoc(&self, room_id: &str, era: &str, joined: bool) -> bool {
        let call_id = signal_calls::call_id_from_era(era);
        let at = now_millis();
        let status = if joined { AdhocCallStatus::Joined } else { AdhocCallStatus::Generic };
        let call = AdhocCall { room_id: room_id.to_owned(), call_id, at, status };
        match self.db.record_adhoc_call(&call).await {
            Ok(changed) => {
                if changed && joined {
                    self.sync_adhoc_call(room_id, call_id, at);
                }
                changed
            }
            Err(error) => {
                warn!(%error, "failed to record a call link's call");
                false
            }
        }
    }

    /// Joins (or starts) a link's call (Desktop's `startCallLinkLobby`, then
    /// join): our admin key comes along for a link of ours.
    pub(super) async fn start_link_call(self: &Rc<Self>, conversation: ConversationId, video: bool) {
        if self.in_any_call() || self.group_call.borrow().is_some() {
            return self.error("can't call", "you're already in a call");
        }
        let Some(room_id) = conversation.as_call_link().map(str::to_owned) else { return };
        let link = match self.db.call_link(&room_id).await {
            Ok(Some(link)) if link.deleted_at.is_none() => link,
            Ok(_) => return self.error("can't join the call", "this call link is gone"),
            Err(error) => return self.error("failed to load the call link", error),
        };
        let Some(manager) = self.manager() else { return };
        if !self.call_session() {
            return self.call_not_started();
        }
        let state = LinkCallState { room_id: room_id.clone(), admin: link.is_admin(), pending: Vec::new(), waiting: false };
        self.begin_link_call(conversation.clone(), video, state);
        self.emit_call();
        let auth = match self.link_auth(&link.root_key).await {
            Ok(auth) => auth,
            Err(error) => {
                self.error("can't join the call", format!("{error:#}"));
                return self.drop_unattached_link_call(&conversation);
            }
        };
        let endorsement = creds::endorsement_key(&manager);
        let mut client = Err(anyhow::anyhow!("no session"));
        self.with_calls(|calls| {
            client = calls.create_call_link_call(&link.root_key, &auth, link.admin_key.clone(), &endorsement);
        });
        let client_id = match client {
            Ok(id) => id,
            Err(error) => {
                self.error("can't join the call", format!("{error:#}"));
                return self.drop_unattached_link_call(&conversation);
            }
        };
        // Hung up while the credential came: let the client go unused.
        if !self.attach_link_client(&conversation, client_id) {
            return self.with_calls(|calls| calls.group_end(client_id));
        }
        self.with_calls(|calls| {
            calls.set_video_enabled(video);
            calls.set_group_audio_muted(client_id, false);
            calls.set_group_video_muted(client_id, !video);
            calls.group_connect(client_id);
            calls.group_join(client_id);
        });
        self.emit_call();
    }

    fn drop_unattached_link_call(&self, conversation: &ConversationId) {
        self.abandon_link_call(conversation);
        self.emit_call();
    }

    /// Lets someone into our link's call, or turns them down.
    pub(super) fn approve_call_join(&self, aci: String, approve: bool) {
        let Some(client_id) = self.link_client_id() else { return };
        let Some(user) = Aci::parse_from_service_id_string(&aci).map(|a| a.service_id_binary()) else { return };
        let user = user.get(..16).map(<[u8]>::to_vec).unwrap_or_default();
        self.with_calls(|calls| if approve { calls.approve_user(client_id, user) } else { calls.deny_user(client_id, user) });
    }

    /// Takes someone out of our link's call.
    pub(super) fn remove_from_call(&self, demux_id: u32) {
        let Some(client_id) = self.link_client_id() else { return };
        self.with_calls(|calls| calls.remove_client(client_id, demux_id));
    }

    /// Desktop's `sendCallLinkUpdateSync`.
    fn sync_call_link_update(&self, root_key: &[u8], admin_key: Option<&[u8]>) {
        let update = sync_message::CallLinkUpdate {
            root_key: Some(root_key.to_vec()),
            admin_passkey: admin_key.map(<[u8]>::to_vec),
            r#type: Some(sync_message::call_link_update::Type::Update.into()),
        };
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::CallLinkUpdate(update)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Desktop's call event sync for a link's call we joined.
    fn sync_adhoc_call(&self, room_id: &str, call_id: i64, at: i64) {
        use presage::proto::sync_message::call_event::{Direction, Event as CallEventKind, Type};
        let Ok(room) = hex::decode(room_id) else { return };
        let event = presage::proto::sync_message::CallEvent {
            conversation_id: Some(room),
            call_id: Some(call_id as u64),
            timestamp: Some(at as u64),
            r#type: Some(Type::AdHocCall.into()),
            // Desktop: no one rings a link's call, so it's always incoming.
            direction: Some(Direction::Incoming.into()),
            event: Some(CallEventKind::Accepted.into()),
        };
        self.enqueue(Outbound::Sync {
            body: SyncMessage {
                content: Some(sync_message::Content::CallEvent(event)),
                ..SyncMessage::with_padding(&mut rand::rng())
            },
        });
    }

    /// Writes a link of ours to Storage Service in the background; a
    /// failure leaves it for the next change (Desktop retries its storage
    /// sync the same way).
    fn spawn_write_call_link(&self, root_key: Vec<u8>, admin_key: Option<Vec<u8>>, deleted_at: u64) {
        let Some(manager) = self.manager() else { return };
        tokio::task::spawn_local(async move {
            if let Err(error) = storage::write_call_link(&manager, &root_key, admin_key.as_deref(), deleted_at).await {
                warn!(error = %format!("{error:#}"), "failed to save the call link to storage");
            }
        });
    }

    /// Another device made or learned of a link (Desktop's
    /// `handleCallLinkUpdate`): keep it, with a placeholder in the history
    /// if it's ours and new, and read its state.
    pub(super) async fn on_call_link_update(self: &Rc<Self>, root_key: Vec<u8>, admin_key: Option<Vec<u8>>) {
        let Some(room) = signal_calls::room_id(&root_key) else { return warn!("call link sync with a bad root key") };
        let room_id = hex::encode(room);
        let link = CallLink { room_id: room_id.clone(), root_key, admin_key, added_at: now_millis(), ..Default::default() };
        match self.db.add_call_link(&link).await {
            Ok(inserted) => {
                if inserted && link.is_admin() {
                    self.add_link_placeholder(&room_id, link.added_at).await;
                }
            }
            Err(error) => return self.error("failed to save the call link", error),
        }
        self.emit(Event::CallLinksChanged);
        self.spawn_refresh_call_links(vec![room_id]);
    }

    /// Another device's record of a link's call.
    pub(super) async fn on_adhoc_call_event(&self, event: crate::mapping::AdhocCallSync) {
        let result = match event.status {
            None => self.db.remove_adhoc_calls(&event.room_id).await.map(|()| true),
            Some(status) => {
                let call = AdhocCall { room_id: event.room_id, call_id: event.call_id, at: event.timestamp, status };
                self.db.record_adhoc_call(&call).await
            }
        };
        match result {
            Ok(true) => self.emit(Event::CallLinksChanged),
            Ok(false) => {}
            Err(error) => warn!(%error, "failed to record a call link's call"),
        }
    }

    /// The newest call through a link, as the call history names it.
    pub(super) async fn latest_link_call(&self) -> Option<signal_store::CallRef> {
        let call = self.db.adhoc_call_history(1).await.ok()?.into_iter().next()?;
        Some(signal_store::CallRef {
            conversation: ConversationId::call_link(&call.room_id),
            call_id: (call.status != AdhocCallStatus::Pending).then_some(call.call_id),
            sent_at: call.at,
        })
    }

    /// Clearing the call history, for links (Desktop's `clearCallHistory`):
    /// their calls up to `up_to` go, but not those of links we're admin of.
    pub(super) async fn clear_link_calls(&self, up_to: i64) {
        match self.db.clear_adhoc_calls(up_to).await {
            Ok(true) => self.emit(Event::CallLinksChanged),
            Ok(false) => {}
            Err(error) => warn!(%error, "failed to clear calls through links"),
        }
    }

    /// Desktop's "Clear call history" here, for links
    /// (`clearCallHistoryDataAndSync`): links we only joined go, and ours
    /// are deleted for everyone, one at a time; one that fails stays (and
    /// Desktop's "Not all call links could be deleted" is shown).
    pub(super) async fn clear_call_links(&self, up_to: i64) {
        self.clear_link_calls(up_to).await;
        let links = match self.db.call_links().await {
            Ok(links) => links,
            Err(error) => return warn!(%error, "failed to read call links"),
        };
        let active = self.link_call_room();
        let mut failed = false;
        for link in links {
            if active.as_deref() == Some(link.room_id.as_str()) {
                failed |= link.is_admin();
                continue;
            }
            if link.is_admin() {
                failed |= !self.delete_admin_link(&link).await;
            } else if let Err(error) = self.db.remove_call_link(&link.room_id).await {
                warn!(%error, "failed to remove a call link");
            }
        }
        self.emit(Event::CallLinksChanged);
        if failed {
            self.emit(Event::Error("Not all call links could be deleted. Check your connection and try again.".into()));
        }
    }

    /// Call link records from Storage Service (Desktop's
    /// `mergeCallLinkRecord`): new ones are kept and read, deleted ones
    /// marked deleted here too.
    pub(super) async fn apply_storage_call_links(self: &Rc<Self>, links: Vec<StorageCallLink>) {
        let mut changed = false;
        let mut fresh = Vec::new();
        for record in links {
            let Some(room) = signal_calls::room_id(&record.root_key) else { continue };
            let room_id = hex::encode(room);
            if let Some(at) = record.deleted_at {
                match self.db.mark_call_link_deleted(&room_id, at).await {
                    Ok(done) => changed |= done,
                    Err(error) => warn!(%error, "failed to mark a call link deleted"),
                }
                continue;
            }
            let admin_key = record.admin_key.clone();
            let link = CallLink { room_id: room_id.clone(), root_key: record.root_key, admin_key, added_at: now_millis(), ..Default::default() };
            match self.db.add_call_link(&link).await {
                Ok(true) => {
                    changed = true;
                    if link.is_admin() {
                        self.add_link_placeholder(&room_id, link.added_at).await;
                    }
                    fresh.push(room_id);
                }
                Ok(false) => {}
                Err(error) => warn!(%error, "failed to save a call link"),
            }
        }
        if changed {
            self.emit(Event::CallLinksChanged);
        }
        self.spawn_refresh_call_links(fresh);
    }

    /// Reads links' names and settings from the calling server, at most
    /// [`REFRESH_PER_RUN`] a run: `rooms` first, then any never read. One
    /// run at a time; one asked for meanwhile runs after it.
    fn spawn_refresh_call_links(self: &Rc<Self>, rooms: Vec<String>) {
        self.link_refresh_queue.borrow_mut().extend(rooms);
        if self.link_refresh_running.replace(true) {
            return;
        }
        let this = self.clone();
        tokio::task::spawn_local(async move {
            loop {
                let mut rooms: Vec<String> = std::mem::take(&mut *this.link_refresh_queue.borrow_mut());
                if let Ok(links) = this.db.call_links().await {
                    rooms.extend(links.into_iter().filter(|l| l.expiration.is_none()).map(|l| l.room_id));
                }
                let mut seen = HashSet::new();
                rooms.retain(|r| seen.insert(r.clone()));
                if rooms.is_empty() {
                    break;
                }
                let mut changed = false;
                let mut failed = false;
                for room_id in rooms.into_iter().take(REFRESH_PER_RUN) {
                    let Ok(Some(link)) = this.db.call_link(&room_id).await else { continue };
                    match this.read_link(&link.root_key).await {
                        Ok(Ok(state)) => changed |= this.save_link_state(&room_id, &state).await,
                        Ok(Err(code)) => warn!(code, "couldn't read a call link"),
                        Err(error) => {
                            warn!(error = %format!("{error:#}"), "couldn't read call links");
                            failed = true;
                            break;
                        }
                    }
                }
                if changed {
                    this.emit(Event::CallLinksChanged);
                }
                // Again only for links asked for meanwhile; the rest wait
                // for the next sync.
                if failed || this.link_refresh_queue.borrow().is_empty() {
                    break;
                }
            }
            this.link_refresh_running.set(false);
        });
    }
}

/// An ACI's string from its 16 bytes.
fn aci_of_bytes(bytes: &[u8]) -> Option<String> {
    let bytes: [u8; 16] = bytes.try_into().ok()?;
    Some(Aci::from_uuid_bytes(bytes).service_id_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cut_to_desktops_limit_on_a_character() {
        assert_eq!(clamp_name("  Book club  "), "Book club");
        let long = "é".repeat(100);
        let cut = clamp_name(&long);
        assert!(cut.len() <= NAME_MAX_BYTES && cut.chars().all(|c| c == 'é'));
        assert_eq!(cut.chars().count(), 60);
    }

    #[test]
    fn restrictions_map_both_ways() {
        for r in [CallLinkRestrictions::None, CallLinkRestrictions::AdminApproval, CallLinkRestrictions::Unknown] {
            assert_eq!(restrictions_of(restrictions_to(r)), r);
        }
    }

    #[test]
    fn failures_read_as_desktop_says_them() {
        assert_eq!(link_error(404), "This call link is no longer valid");
        assert_eq!(link_error(703), "This call link is no longer valid");
        assert!(link_error(500).contains("500"));
    }
}
