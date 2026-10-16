//! Call links, through RingRTC's call link API (as Desktop's
//! `ts/services/calling.preload.ts` uses it): making a link's root key and
//! admin passkey, its `https://signal.link/call/#key=…` URL, and creating,
//! reading, changing, deleting and peeking a link's call on the calling
//! server. Requests go out through the app (`LinkClient::new`'s `send`), as
//! group calls' do.

use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;

use rand::rand_core::UnwrapErr;
use ringrtc::lite::call_links::{
    self, CallLinkDeleteRequest, CallLinkMemberResolver, CallLinkRestrictions, CallLinkRootKey, CallLinkState,
    CallLinkUpdateRequest,
};
use ringrtc::lite::http::{self, DelegatingClient};
use ringrtc::lite::sfu::{self, PeekArgs};

use crate::group::Peek;
use crate::{HttpRequest, HttpResponse, SFU_URL};

/// Desktop's call link URLs (`callLinkRootKeyToUrl`).
pub const LINK_PREFIX: &str = "https://signal.link/call/#key=";

/// Who may join a link's call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkRestrictions {
    None,
    AdminApproval,
    #[default]
    Unknown,
}

impl From<&CallLinkRestrictions> for LinkRestrictions {
    fn from(value: &CallLinkRestrictions) -> Self {
        match value {
            CallLinkRestrictions::None => Self::None,
            CallLinkRestrictions::AdminApproval => Self::AdminApproval,
            CallLinkRestrictions::Unknown => Self::Unknown,
        }
    }
}

impl LinkRestrictions {
    fn to_ringrtc(self) -> Option<CallLinkRestrictions> {
        match self {
            Self::None => Some(CallLinkRestrictions::None),
            Self::AdminApproval => Some(CallLinkRestrictions::AdminApproval),
            Self::Unknown => None,
        }
    }
}

/// A link's state on the calling server (RingRTC's `CallLinkState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkState {
    pub name: String,
    pub restrictions: LinkRestrictions,
    pub revoked: bool,
    /// Unix millis.
    pub expiration: i64,
    /// The root key as the server left it: creating a link gives it its
    /// epoch, so a new link's key is this one, not the one sent.
    pub root_key: Vec<u8>,
}

impl From<&CallLinkState> for LinkState {
    fn from(state: &CallLinkState) -> Self {
        let expiration = state.expiration.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        Self {
            name: state.name.clone(),
            restrictions: LinkRestrictions::from(&state.restrictions),
            revoked: state.revoked,
            expiration,
            root_key: state.root_key.as_slice().to_vec(),
        }
    }
}

/// A failed request: the HTTP status, or RingRTC's own codes (703: the
/// link expired, 704: it's not valid, 602: the request failed).
pub type LinkResult<T> = Result<T, u16>;

/// A new link's root key (not valid until the server has created it) and
/// admin passkey, as Desktop's `CallLinkRootKey.generate()` and
/// `generateAdminPassKey()`.
pub fn new_link_keys() -> (Vec<u8>, Vec<u8>) {
    let key = CallLinkRootKey::generate(UnwrapErr(rand::rngs::SysRng));
    let passkey = CallLinkRootKey::generate_admin_passkey(UnwrapErr(rand::rngs::SysRng));
    (key.as_slice().to_vec(), passkey)
}

fn root_key(bytes: &[u8]) -> Option<CallLinkRootKey> {
    CallLinkRootKey::try_from(bytes).ok()
}

/// The room a root key names (Desktop's `roomId`, in hex).
pub fn room_id(root_key_bytes: &[u8]) -> Option<Vec<u8>> {
    root_key(root_key_bytes).map(|k| k.derive_room_id())
}

/// A root key as people see it (Desktop's `toUnredactedString`):
/// consonant letters in dashed groups.
pub fn root_key_string(root_key_bytes: &[u8]) -> Option<String> {
    root_key(root_key_bytes).map(|k| k.to_formatted_string())
}

/// A link's URL, as Desktop shares it.
pub fn link_url(root_key_bytes: &[u8]) -> Option<String> {
    Some(format!("{LINK_PREFIX}{}", root_key_string(root_key_bytes)?))
}

/// The root key in a call link's URL (Desktop's `getKeyFromCallLink`: the
/// `key` of the URL's fragment), if it's one.
pub fn parse_link(url: &str) -> Option<Vec<u8>> {
    let url = url.trim();
    let rest = url.strip_prefix("https://signal.link/call/").or_else(|| url.strip_prefix("sgnl://signal.link/call/"))?;
    let fragment = rest.split_once('#')?.1;
    let key = fragment.split('&').find_map(|pair| pair.strip_prefix("key="))?;
    let key = CallLinkRootKey::try_from(key).ok()?;
    key.is_valid().then(|| key.as_slice().to_vec())
}

/// Hands a response back to the request it answers.
#[derive(Clone, Default)]
pub struct Responder(Arc<OnceLock<DelegatingClient>>);

impl Responder {
    pub fn respond(&self, request_id: u32, response: Option<HttpResponse>) {
        if let Some(client) = self.0.get() {
            client.received_response(request_id, response);
        }
    }
}

type Send_ = Arc<dyn Fn(u32, HttpRequest, Responder) + Send + Sync>;

struct Delegate {
    send: Send_,
    responder: Responder,
}

impl http::Delegate for Delegate {
    fn send_request(&self, request_id: u32, request: HttpRequest) {
        (self.send)(request_id, request, self.responder.clone());
    }
}

/// The calling server's call link API. Each call's `done` runs once, on
/// whichever thread hands the response over.
#[derive(Clone)]
pub struct LinkClient {
    http: DelegatingClient,
}

impl LinkClient {
    /// `send` makes a request and hands its response (`None`: it failed)
    /// to the `Responder`, from any thread.
    pub fn new(send: impl Fn(u32, HttpRequest, Responder) + Send + Sync + 'static) -> Self {
        let responder = Responder::default();
        let http = DelegatingClient::new(Delegate { send: Arc::new(send), responder: responder.clone() });
        let _ = responder.0.set(http.clone());
        Self { http }
    }

    /// Creates a link (Desktop's `createCallLink`, which asks for admin
    /// approval). `presentation` is a `CreateCallLinkCredentialPresentation`
    /// for the room, `public_params` the link's `CallLinkPublicParams`.
    pub fn create(
        &self,
        root_key_bytes: &[u8],
        presentation: &[u8],
        admin_passkey: &[u8],
        public_params: &[u8],
        restrictions: LinkRestrictions,
        done: impl FnOnce(LinkResult<LinkState>) + Send + 'static,
    ) {
        let Some(key) = root_key(root_key_bytes) else { return done(Err(http::ResponseStatus::CALL_LINK_INVALID.code)) };
        call_links::create_call_link(
            &self.http,
            SFU_URL,
            key,
            presentation,
            admin_passkey,
            public_params,
            restrictions.to_ringrtc(),
            Box::new(move |result| done(result.as_ref().map(LinkState::from).map_err(|s| s.code))),
        );
    }

    /// A link's name, approval setting and expiration (Desktop's
    /// `readCallLink`). `auth` is a `CallLinkAuthCredentialPresentation`.
    pub fn read(&self, root_key_bytes: &[u8], auth: &[u8], done: impl FnOnce(LinkResult<LinkState>) + Send + 'static) {
        let Some(key) = root_key(root_key_bytes) else { return done(Err(http::ResponseStatus::CALL_LINK_INVALID.code)) };
        call_links::read_call_link(
            &self.http,
            SFU_URL,
            key,
            auth,
            Box::new(move |result| done(result.as_ref().map(LinkState::from).map_err(|s| s.code))),
        );
    }

    /// Renames a link (empty: no name) or changes who may join, as an
    /// admin (Desktop's `updateCallLinkName` / `updateCallLinkRestrictions`).
    pub fn update(
        &self,
        root_key_bytes: &[u8],
        auth: &[u8],
        admin_passkey: &[u8],
        name: Option<&str>,
        restrictions: Option<LinkRestrictions>,
        done: impl FnOnce(LinkResult<LinkState>) + Send + 'static,
    ) {
        let Some(key) = root_key(root_key_bytes) else { return done(Err(http::ResponseStatus::CALL_LINK_INVALID.code)) };
        // As RingRTC's own bindings: an empty name is sent empty, not encrypted.
        let encrypted = name.map(|n| if n.is_empty() { Vec::new() } else { key.encrypt(n.as_bytes(), UnwrapErr(rand::rngs::SysRng)) });
        let request = CallLinkUpdateRequest {
            admin_passkey,
            encrypted_name: encrypted.as_deref(),
            restrictions: restrictions.and_then(LinkRestrictions::to_ringrtc),
            revoked: None,
        };
        call_links::update_call_link(
            &self.http,
            SFU_URL,
            key,
            auth,
            &request,
            Box::new(move |result| done(result.as_ref().map(LinkState::from).map_err(|s| s.code))),
        );
    }

    /// Deletes a link, as its admin (Desktop's `deleteCallLink`).
    pub fn delete(&self, root_key_bytes: &[u8], auth: &[u8], admin_passkey: &[u8], done: impl FnOnce(LinkResult<()>) + Send + 'static) {
        let Some(key) = root_key(root_key_bytes) else { return done(Err(http::ResponseStatus::CALL_LINK_INVALID.code)) };
        call_links::delete_call_link(
            &self.http,
            SFU_URL,
            key,
            auth,
            &CallLinkDeleteRequest { admin_passkey },
            Box::new(move |result| done(result.map(|_| ()).map_err(|s| s.code))),
        );
    }

    /// Who's in a link's call, and the link's state (Desktop's
    /// `peekCallLinkCall`). No call: `Ok` with no one in it.
    pub fn peek(&self, root_key_bytes: &[u8], auth: &[u8], done: impl FnOnce(LinkResult<Peek>) + Send + 'static) {
        let Some(key) = root_key(root_key_bytes) else { return done(Err(http::ResponseStatus::CALL_LINK_INVALID.code)) };
        sfu::peek(
            &self.http,
            SFU_URL,
            PeekArgs {
                room_id_header: None,
                auth_header: call_links::auth_header_from_auth_credential(auth),
                member_resolver: Arc::new(CallLinkMemberResolver::from(&key)),
                call_link_root_key: Some(key),
            },
            Box::new(move |result| done(result.as_ref().map(Peek::from).map_err(|s| s.code))),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_keys_are_not_valid_until_created_and_name_a_room() {
        let (key, passkey) = new_link_keys();
        assert_eq!(passkey.len(), 16);
        // A V1 key without its epoch: it names a room, but isn't a link yet.
        assert_eq!(room_id(&key).map(|r| r.len()), Some(32));
        let url = link_url(&key).unwrap();
        assert!(url.starts_with(LINK_PREFIX));
        assert_eq!(parse_link(&url), None, "not valid before the server gives it an epoch");
    }

    #[test]
    fn links_round_trip_through_their_urls() {
        // A V0 key (16 bytes), as links made before epochs have.
        let key: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let url = link_url(&key).unwrap();
        assert_eq!(parse_link(&url), Some(key.clone()));
        assert_eq!(parse_link(&format!("{url}&epoch=1")), Some(key.clone()));
        assert_eq!(parse_link("https://signal.link/call/#key=nope"), None);
        assert_eq!(parse_link("https://example.com/call/#key=abc"), None);
        // The same room, however it's written.
        assert_eq!(room_id(&key), room_id(&parse_link(&url).unwrap()));
    }

    #[test]
    fn requests_go_through_the_app_and_answers_come_back() {
        use std::sync::Mutex;
        let sent: Arc<Mutex<Vec<(u32, HttpRequest, Responder)>>> = Arc::default();
        let log = sent.clone();
        let client = LinkClient::new(move |id, request, responder| log.lock().unwrap().push((id, request, responder)));
        let key: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let got: Arc<Mutex<Option<LinkResult<LinkState>>>> = Arc::default();
        let out = got.clone();
        client.read(&key, &[1, 2, 3], move |result| *out.lock().unwrap() = Some(result));
        let (id, request, responder) = sent.lock().unwrap().pop().unwrap();
        assert_eq!(request.url, "https://sfu.voip.signal.org/v1/call-link");
        assert!(request.headers.contains_key("X-Room-Id"));
        assert_eq!(request.headers["Authorization"], "Bearer auth.AQID");
        let body = br#"{"name":"","restrictions":"adminApproval","revoked":false,"expiration":1700000000}"#;
        responder.respond(id, Some(HttpResponse { status: 200.into(), body: body.to_vec() }));
        let state = got.lock().unwrap().take().unwrap().unwrap();
        assert_eq!((state.restrictions, state.expiration), (LinkRestrictions::AdminApproval, 1_700_000_000_000));
        assert_eq!(state.root_key, key);

        // A missing link reads as its status.
        client.read(&key, &[1], {
            let out = got.clone();
            move |result| *out.lock().unwrap() = Some(result)
        });
        let (id, _, responder) = sent.lock().unwrap().pop().unwrap();
        responder.respond(id, Some(HttpResponse { status: 404.into(), body: Vec::new() }));
        assert_eq!(got.lock().unwrap().take().unwrap(), Err(404));
    }
}
