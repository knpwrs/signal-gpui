//! Signal's links (`sgnl://…`, `https://signal.me/…`, `signal.group`,
//! `signal.link/call`, `signal.art/addstickers`), read as Signal Desktop's
//! `ts/util/signalRoutes.std.ts` reads them. Desktop matches each route with
//! a `URLPattern`: exact scheme and host, an empty or `/` path (or the
//! route's own), and a hash or query that the whole pattern must cover.
//! A link that matches none isn't Signal's, and opens in the browser.

use url::Url;

/// What a link asks for (Desktop's `ParsedSignalRoute`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `signal.me/#p/+15551234567`: a chat with that number
    /// (`contactByPhoneNumberRoute`).
    ContactByPhoneNumber(String),
    /// `signal.me/#eu/<base64url>`: a chat with whoever owns that username
    /// link (`contactByEncryptedUsernameRoute`).
    ContactByEncryptedUsername(String),
    /// `signal.group/#<invite>`, or `sgnl://joingroup/#<invite>`
    /// (`groupInvitesRoute`): the invite code.
    GroupInvite(String),
    /// `signal.link/call/#key=…` (`linkCallRoute`): the root key.
    LinkCall(String),
    /// `signal.art/addstickers/#pack_id=…&pack_key=…` or
    /// `sgnl://addstickers/?pack_id=…&pack_key=…` (`artAddStickersRoute`):
    /// hex id and key.
    AddStickers { pack_id: String, pack_key: String },
    /// `sgnl://show-window`.
    ShowWindow,
    /// `signalcaptcha://<captcha>` (`captchaRoute`, which takes the
    /// hostname): a solved captcha, for the challenge waiting on it.
    Captcha(String),
    /// Routes Desktop knows that this app has no use for: linking a device
    /// (we're never the primary), its own
    /// notification tokens (`show-conversation`, `start-call-lobby`),
    /// `cancel-presenting` and donations. Desktop shows its "didn't make
    /// sense" error for the ones it doesn't handle.
    Unsupported(&'static str),
}

impl Route {
    /// The link Desktop would open for it on the web (`toWebUrl`), for the
    /// handlers here that take a URL.
    pub fn group_link(code: &str) -> String {
        format!("https://signal.group/#{code}")
    }

    pub fn call_link(key: &str) -> String {
        format!("https://signal.link/call/#key={key}")
    }

    pub fn username_link(encrypted: &str) -> String {
        format!("https://signal.me/#eu/{encrypted}")
    }
}

/// Desktop's "Sorry, that sgnl:// link didn't make sense!"
/// (`icu:unknown-sgnl-link`), under "Something went wrong!".
pub const UNKNOWN_LINK_TITLE: &str = "Something went wrong!";
pub const UNKNOWN_LINK: &str = "Sorry, that sgnl:// link didn't make sense!";

/// Desktop's `parseSignalRoute`: the route `input` names, if any.
pub fn parse(input: &str) -> Option<Route> {
    let url = Url::parse(input.trim()).ok()?;
    // `URLPattern` requires these empty.
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return None;
    }
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("");
    let path = url.path();
    let hash = url.fragment().unwrap_or("");
    let search = url.query().unwrap_or("");
    // `{/}?`: no path, or just "/".
    let bare = path.is_empty() || path == "/";
    let at = |route: &str| path == route || path.strip_suffix('/') == Some(route);

    match (scheme, host) {
        ("https" | "sgnl", "signal.me") if bare => {
            if let Some(number) = hash.strip_prefix("p/") {
                return is_e164(number).then(|| Route::ContactByPhoneNumber(number.to_owned()));
            }
            let encrypted = hash.strip_prefix("eu/").filter(|e| !e.is_empty())?;
            Some(Route::ContactByEncryptedUsername(encrypted.to_owned()))
        }
        ("https" | "sgnl", "signal.group") | ("sgnl", "joingroup") if bare => {
            (!hash.is_empty() && !hash.contains('/')).then(|| Route::GroupInvite(hash.to_owned()))
        }
        ("https" | "sgnl", "signal.link") if at("/call") => {
            let key = param(hash, "key")?;
            Some(Route::LinkCall(key))
        }
        ("https", "signal.art") if at("/addstickers") => stickers(hash),
        ("sgnl", "addstickers") if bare => stickers(search),
        ("sgnl", "show-window") if bare && hash.is_empty() && search.is_empty() => Some(Route::ShowWindow),
        ("sgnl", "linkdevice") if bare && !search.is_empty() => Some(Route::Unsupported("linkDevice")),
        ("sgnl", "show-conversation") if bare && param(search, "token").is_some() => {
            Some(Route::Unsupported("showConversation"))
        }
        ("sgnl", "start-call-lobby") if bare && param(search, "token").is_some() => {
            Some(Route::Unsupported("startCallLobby"))
        }
        ("sgnl", "cancel-presenting") if bare && hash.is_empty() && search.is_empty() => {
            Some(Route::Unsupported("cancelPresenting"))
        }
        ("sgnl", "donation-validation-complete" | "paypal") if bare && !search.is_empty() => {
            Some(Route::Unsupported("donation"))
        }
        ("signalcaptcha", captcha) if !captcha.is_empty() => Some(Route::Captcha(captcha.to_owned())),
        _ => None,
    }
}

/// A `pack_id=…&pack_key=…` pair, both present (Desktop's schema: non-empty
/// strings).
fn stickers(params: &str) -> Option<Route> {
    Some(Route::AddStickers { pack_id: param(params, "pack_id")?, pack_key: param(params, "pack_key")? })
}

/// A non-empty `URLSearchParams` value.
fn param(params: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(params.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// Desktop's `isValidE164(value, true)`: a `+`, then a number
/// libphonenumber calls valid.
fn is_e164(number: &str) -> bool {
    number.starts_with('+')
        && number[1..].chars().all(|c| c.is_ascii_digit())
        && crate::recipients::phone_in(number, None).as_deref() == Some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_me_links_name_a_number_or_a_username_link() {
        for url in ["https://signal.me/#p/+14155550123", "sgnl://signal.me/#p/+14155550123", "https://signal.me#p/+14155550123"] {
            assert_eq!(parse(url), Some(Route::ContactByPhoneNumber("+14155550123".into())), "{url}");
        }
        assert_eq!(parse("https://signal.me/#p/14155550123"), None, "needs the +");
        assert_eq!(parse("https://signal.me/#p/+1415"), None, "not a valid number");
        assert_eq!(parse("https://signal.me/#p/"), None);
        assert_eq!(
            parse("https://signal.me/#eu/ABC-_def"),
            Some(Route::ContactByEncryptedUsername("ABC-_def".into()))
        );
        assert_eq!(parse("sgnl://signal.me/#eu/xyz"), Some(Route::ContactByEncryptedUsername("xyz".into())));
        assert_eq!(parse("https://signal.me/#eu/"), None);
        assert_eq!(parse("https://signal.me"), None, "Desktop: no route without a hash");
        assert_eq!(parse("https://signal.me/other#p/+14155550123"), None, "only an empty path");
        assert_eq!(parse("http://signal.me/#p/+14155550123"), None, "https only");
        assert_eq!(parse("https://user@signal.me/#p/+14155550123"), None);
        assert_eq!(parse("https://signal.me:8443/#p/+14155550123"), None);
    }

    #[test]
    fn group_links_keep_their_invite_code() {
        for url in ["https://signal.group/#CjQKIA", "sgnl://signal.group/#CjQKIA", "sgnl://joingroup/#CjQKIA", "https://signal.group#CjQKIA"] {
            assert_eq!(parse(url), Some(Route::GroupInvite("CjQKIA".into())), "{url}");
        }
        assert_eq!(parse("https://signal.group/#a/b"), None, "no slash in the code");
        assert_eq!(parse("https://signal.group/"), None);
        assert_eq!(Route::group_link("CjQKIA"), "https://signal.group/#CjQKIA");
        assert!(crate::group_admin::is_group_link(&Route::group_link("CjQKIA")));
    }

    #[test]
    fn call_links_take_the_key_from_the_hash() {
        let key = "bcdf-ghkm-npqr-stxz-bcdf-ghkm-npqr-stxz";
        for url in [
            format!("https://signal.link/call/#key={key}"),
            format!("https://signal.link/call#key={key}"),
            format!("sgnl://signal.link/call/#key={key}"),
        ] {
            assert_eq!(parse(&url), Some(Route::LinkCall(key.into())), "{url}");
        }
        assert_eq!(parse("https://signal.link/call/#other=1"), None);
        assert_eq!(parse("https://signal.link/call/"), None);
        assert_eq!(parse(&format!("https://signal.link/#key={key}")), None, "not a call");
        // Proxy links (signal.tube) aren't routes: Desktop opens them in the browser.
        assert_eq!(parse("https://signal.tube/#proxy.example.com"), None);
        assert_eq!(Route::call_link(key), format!("https://signal.link/call/#key={key}"));
    }

    #[test]
    fn sticker_links_have_both_halves() {
        let want = Some(Route::AddStickers { pack_id: "0123abcd".into(), pack_key: "ffee".into() });
        assert_eq!(parse("https://signal.art/addstickers/#pack_id=0123abcd&pack_key=ffee"), want);
        assert_eq!(parse("https://signal.art/addstickers#pack_key=ffee&pack_id=0123abcd"), want);
        assert_eq!(parse("sgnl://addstickers/?pack_id=0123abcd&pack_key=ffee"), want);
        assert_eq!(parse("sgnl://addstickers?pack_id=0123abcd&pack_key=ffee"), want);
        assert_eq!(parse("https://signal.art/addstickers/#pack_id=0123abcd"), None);
        assert_eq!(parse("https://signal.art/addstickers/#pack_id=&pack_key=ffee"), None);
        assert_eq!(parse("sgnl://addstickers#pack_id=0123abcd&pack_key=ffee"), None, "sgnl uses the query");
        // Our own pack creator's links read back.
        let ours = crate::sticker_upload::share_url(&[1, 35], &[255, 238]);
        assert_eq!(parse(&ours), Some(Route::AddStickers { pack_id: "0123".into(), pack_key: "ffee".into() }));
    }

    #[test]
    fn other_routes_are_recognized_but_unsupported() {
        assert_eq!(parse("sgnl://show-window"), Some(Route::ShowWindow));
        assert_eq!(parse("sgnl://show-window/"), Some(Route::ShowWindow));
        assert_eq!(parse("sgnl://linkdevice?uuid=a&pub_key=b"), Some(Route::Unsupported("linkDevice")));
        assert_eq!(parse("signalcaptcha://signal-hcaptcha.abc"), Some(Route::Captcha("signal-hcaptcha.abc".into())));
        // Desktop's own test: a trailing slash is fine.
        let id = "signal-hcaptcha.Foo-bAr_baz.challenge.fOo-bAR_baZ.fOO-BaR_baz";
        assert_eq!(parse(&format!("signalcaptcha://{id}/")), Some(Route::Captcha(id.into())));
        assert_eq!(parse("sgnl://show-conversation?token=t"), Some(Route::Unsupported("showConversation")));
        assert_eq!(parse("sgnl://start-call-lobby?token=t"), Some(Route::Unsupported("startCallLobby")));
        assert_eq!(parse("sgnl://cancel-presenting"), Some(Route::Unsupported("cancelPresenting")));
        assert_eq!(parse("sgnl://paypal?action=cancel&returnToken=x"), Some(Route::Unsupported("donation")));
        for url in ["sgnl://nonsense", "sgnl://signal.me", "https://example.com/#p/+14155550123", "not a url", ""] {
            assert_eq!(parse(url), None, "{url}");
        }
    }
}
