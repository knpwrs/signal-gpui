//! Link previews, following Signal Desktop (`ts/types/LinkPreview.std.ts`,
//! `ts/linkPreviews/linkPreviewFetch.preload.ts`):
//!
//! - Only https links with a real, plain domain are previewed; anything that
//!   looks like it could be hiding something ("sneaky") is not.
//! - A received preview is shown only if its link appears in the message.
//! - Generating one fetches the page directly (as Desktop does), reading at
//!   most 1 MB of HTML, and its image (at most 1 MB). Every redirect is
//!   checked against the same rules.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use scraper::{Html, Selector};
use url::Url;

use crate::media;

/// Desktop identifies as WhatsApp, which many sites serve good previews to.
const USER_AGENT: &str = "WhatsApp/2";
const MAX_REDIRECTS: usize = 20;
const MAX_HTML_BYTES: usize = 1000 * 1024;
const MAX_IMAGE_BYTES: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_HREF_LENGTH: usize = 4096;

const EXCLUDED_DOMAINS: &[&str] = &[
    "debuglogs.org",
    "example",
    "example.com",
    "example.net",
    "example.org",
    "invalid",
    "localhost",
    "onion",
    "test",
];

/// A preview ready to show in the composer and send.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkPreviewDraft {
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    /// Unix millis.
    pub date: Option<i64>,
    pub image: Option<PreviewImage>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewImage {
    pub bytes: Arc<Vec<u8>>,
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Whether a link may be previewed (Desktop's `shouldPreviewHref`).
pub fn should_preview(href: &str) -> bool {
    let Ok(url) = Url::parse(href) else { return false };
    url.scheme() == "https" && !is_domain_excluded(&url) && !is_sneaky(href, &url)
}

fn is_domain_excluded(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    EXCLUDED_DOMAINS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// Desktop's `isLinkSneaky`, where "maybe" also counts.
fn is_sneaky(href: &str, url: &Url) -> bool {
    if href.len() > MAX_HREF_LENGTH || href.chars().any(|c| ('\u{2500}'..='\u{25FF}').contains(&c)) {
        return true;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    // Only names, not IP addresses.
    let Some(url::Host::Domain(host)) = url.host() else { return true };
    if host.len() > 2048 || host.contains('%') {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return true;
    }
    // Mixing ASCII with other scripts in a domain is a classic disguise.
    let (unicode, _) = idna::domain_to_unicode(host);
    let letters: String = unicode.chars().filter(|c| *c != '.').collect();
    let has_ascii = letters.chars().any(|c| (' '..='\u{7f}').contains(&c));
    let has_other = letters.chars().any(|c| !(' '..='\u{7f}').contains(&c));
    if has_ascii && has_other {
        return true;
    }
    // The path and fragment, as written, must be plain URI characters.
    let after_scheme = href.find("://").map_or(0, |i| i + 3);
    let path = href[after_scheme..].find('/').map_or("", |i| &href[after_scheme + i..]);
    !path.chars().all(|c| c.is_ascii_alphanumeric() || "%:/?#[]@!$&'()*+,;=-._~".contains(c))
}

/// Links in a message, as written (what a preview's URL must match).
pub fn links_in(text: &str) -> Vec<String> {
    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    finder.links(text).map(|link| link.as_str().to_owned()).collect()
}

/// The first link in `text` worth previewing.
pub fn first_previewable(text: &str) -> Option<String> {
    links_in(text).into_iter().find(|link| should_preview(link))
}

/// Whether a received preview may be shown (Desktop's `isValidLinkPreview`).
pub fn is_valid(body: &str, url: &str) -> bool {
    should_preview(url) && links_in(body).iter().any(|link| link == url)
}

/// What a page says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
    pub title: String,
    pub description: Option<String>,
    pub image: Option<String>,
    pub date: Option<i64>,
}

static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static META_DESCRIPTION: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"meta[name="description"]"#).unwrap());

fn meta_property(document: &Html, properties: &[&str]) -> Option<String> {
    properties.iter().find_map(|property| {
        let selector = Selector::parse(&format!(r#"meta[property="{property}"]"#)).ok()?;
        let content = document.select(&selector).next()?.value().attr("content")?.trim();
        (!content.is_empty()).then(|| content.to_owned())
    })
}

fn link_rel(document: &Html, rels: &[&str]) -> Option<String> {
    rels.iter().find_map(|rel| {
        let selector = Selector::parse(&format!(r#"link[rel="{rel}"]"#)).ok()?;
        let href = document.select(&selector).next()?.value().attr("href")?.trim();
        (!href.is_empty()).then(|| href.to_owned())
    })
}

/// Desktop's `parseMetadata`: Open Graph first, then plain HTML.
pub fn parse_metadata(html: &str, page: &Url) -> Option<Metadata> {
    let document = Html::parse_document(html);
    let title = meta_property(&document, &["og:title"]).or_else(|| {
        let title = document.select(&TITLE).next()?.text().collect::<String>();
        let title = title.trim();
        (!title.is_empty()).then(|| title.to_owned())
    })?;
    let description = meta_property(&document, &["og:description"]).or_else(|| {
        let content = document.select(&META_DESCRIPTION).next()?.value().attr("content")?.trim();
        (!content.is_empty()).then(|| content.to_owned())
    });
    let image = meta_property(&document, &["og:image", "og:image:url"])
        .or_else(|| link_rel(&document, &["apple-touch-icon", "apple-touch-icon-precomposed", "shortcut icon", "icon"]))
        .and_then(|href| page.join(&href).ok())
        .map(|url| url.to_string());
    let date = meta_property(
        &document,
        &["og:modified_time", "article:modified_time", "og:published_time", "article:published_time"],
    )
    .and_then(|raw| parse_date(&raw));
    Some(Metadata { title, description, image, date })
}

/// RFC 3339 dates, or plain `YYYY-MM-DD`, between 1970 and 3000.
fn parse_date(raw: &str) -> Option<i64> {
    let millis = chrono::DateTime::parse_from_rfc3339(raw)
        .map(|d| d.timestamp_millis())
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(raw.get(..10)?, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc().timestamp_millis())
        })?;
    let max = chrono::NaiveDate::from_ymd_opt(3000, 1, 1)?.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis();
    (millis > 0 && millis < max).then_some(millis)
}

/// TLS for arbitrary sites: ring, checked against the system trust store.
pub(crate) fn tls_config() -> anyhow::Result<rustls::ClientConfig> {
    use rustls_platform_verifier::BuilderVerifierExt as _;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth())
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(tls_config()?)
        .dns_resolver(PublicOnly)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()?)
}

/// Resolves names to their public addresses only, so a page (or its image,
/// or a redirect) can't point us at the local network or the machine
/// itself. The client connects only to what this returns, so a second
/// lookup can't slip a private address past the check.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| is_public(a.ip())).collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Reachable on the public internet: not loopback, private, link-local,
/// carrier-grade NAT, documentation, benchmarking, multicast or reserved.
pub(crate) fn is_public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (18..20).contains(&b))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            // NAT64 (64:ff9b::/96) reaches the IPv4 address inside it.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = s[6].to_be_bytes();
                let [c, d] = s[7].to_be_bytes();
                return is_public(IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d)));
            }
            let first = s[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                // Old site-local, and 6to4 (which tunnels to any IPv4 address).
                || (first & 0xffc0) == 0xfec0
                || first == 0x2002
                || (first == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// GET with manual redirects, each target re-checked.
async fn get(client: &reqwest::Client, href: &str, accept: &str) -> anyhow::Result<reqwest::Response> {
    let mut next = href.to_owned();
    for _ in 0..MAX_REDIRECTS {
        let response = client.get(&next).header("Accept", accept).send().await?;
        if !response.status().is_redirection() {
            return Ok(response.error_for_status()?);
        }
        let location = response
            .headers()
            .get("Location")
            .and_then(|l| l.to_str().ok())
            .context("redirect without a location")?;
        let target = Url::parse(&next)?.join(location)?.to_string();
        if !should_preview(&target) {
            bail!("redirect to a link we don't preview");
        }
        next = target;
    }
    bail!("too many redirects")
}

/// Reads at most `limit` bytes of the body.
async fn read_capped(mut response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= limit {
            break;
        }
    }
    Ok(body)
}

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Fetches and builds a preview, or `None` when the page doesn't offer one.
pub async fn fetch(href: &str, level: media::Optimization) -> anyhow::Result<Option<LinkPreviewDraft>> {
    if !should_preview(href) {
        return Ok(None);
    }
    let client = client()?;
    let page = get(&client, href, "text/html,application/xhtml+xml").await?;
    if !matches!(content_type(&page).as_str(), "text/html" | "application/xhtml+xml") {
        return Ok(None);
    }
    let base = page.url().clone();
    let html = read_capped(page, MAX_HTML_BYTES).await?;
    let Some(metadata) = parse_metadata(&String::from_utf8_lossy(&html), &base) else { return Ok(None) };

    // A preview without its image is still worth sending.
    let image = match metadata.image.as_deref().filter(|href| should_preview(href)) {
        Some(image_href) => fetch_image(&client, image_href, level).await.unwrap_or_else(|error| {
            tracing::debug!(%error, "link preview image unavailable");
            None
        }),
        None => None,
    };
    Ok(Some(LinkPreviewDraft {
        url: href.to_owned(),
        title: metadata.title,
        description: metadata.description,
        date: metadata.date,
        image,
    }))
}

async fn fetch_image(
    client: &reqwest::Client,
    href: &str,
    level: media::Optimization,
) -> anyhow::Result<Option<PreviewImage>> {
    let response = get(client, href, "image/*").await?;
    let kind = content_type(&response);
    if !matches!(
        kind.as_str(),
        "image/gif" | "image/x-icon" | "image/vnd.microsoft.icon" | "image/jpeg" | "image/png" | "image/webp"
    ) {
        return Ok(None);
    }
    if response.content_length().is_some_and(|len| len as usize > MAX_IMAGE_BYTES) {
        return Ok(None);
    }
    let bytes = read_capped(response, MAX_IMAGE_BYTES + 1).await?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Ok(None);
    }
    // Same lossless treatment as any image we send; icons become PNG.
    let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<media::PreparedImage> {
        let bytes = if bytes.starts_with(&[0, 0, 1, 0]) { ico_to_png(&bytes)? } else { media::normalize(bytes)? };
        media::prepare(&bytes, level)
    })
    .await
    .map_err(|e| anyhow!("{e}"))??;
    Ok(Some(PreviewImage {
        bytes: Arc::new(prepared.bytes),
        content_type: prepared.content_type,
        width: prepared.width,
        height: prepared.height,
    }))
}

fn ico_to_png(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico)?;
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_fetched() {
        for ip in [
            "10.0.0.1", "192.168.1.1", "127.0.0.1", "169.254.1.1", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1",
            "::ffff:10.0.0.1", "64:ff9b::7f00:1", "2002:a00:1::1", "fec0::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.216.34", "2606:2800:220:1::1", "::ffff:93.184.216.34", "64:ff9b::5db8:d822"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    /// Hits the network: `cargo test -p signal-backend -- --ignored live`.
    #[tokio::test]
    #[ignore]
    async fn live_fetch_of_a_real_page() {
        let preview = fetch("https://signal.org/", media::Optimization::Max).await.unwrap().expect("signal.org has a preview");
        assert!(!preview.title.is_empty());
        let image = preview.image.expect("signal.org has an og:image");
        assert!(image.width > 0 && !image.bytes.is_empty());
        println!("{} | {:?} | {}x{} {}", preview.title, preview.description, image.width, image.height, image.content_type);
    }

    #[test]
    fn previewable_links_follow_desktop() {
        assert!(should_preview("https://signal.org/blog/"));
        assert!(should_preview("https://x.com/NamorPB/status/2110369560763470007"));
        for href in [
            "http://signal.org/",                 // not https
            "https://localhost/a",                // excluded
            "https://www.example.com/",           // excluded
            "https://foo.onion/",                 // excluded
            "https://user:pw@signal.org/",        // credentials
            "https://127.0.0.1/",                 // IP address
            "https://signal/",                    // one label
            "https://xn--sgnal-m3a.org/",         // mixes scripts (sıgnal)
            "https://signal.org/a b",             // unusual path characters
            "https://signal.org/\u{2500}",        // box drawing
        ] {
            assert!(!should_preview(href), "{href}");
        }
    }

    #[test]
    fn received_previews_must_match_a_link_in_the_message() {
        let body = "I woke up and chose violence: https://x.com/NamorPB/status/1";
        assert!(is_valid(body, "https://x.com/NamorPB/status/1"));
        assert!(!is_valid(body, "https://evil.example.org/"));
        assert!(!is_valid("no links", "https://x.com/NamorPB/status/1"));
        assert_eq!(first_previewable("see http://a.org and https://b.org/x."), Some("https://b.org/x".into()));
    }

    #[test]
    fn metadata_prefers_open_graph_and_resolves_images() {
        let page = Url::parse("https://news.site/articles/1").unwrap();
        let html = r#"<html><head>
            <title>Fallback title</title>
            <meta property="og:title" content=" Tom &amp; Jerry ">
            <meta name="description" content="plain description">
            <meta property="og:image" content="/img/cover.png">
            <meta property="article:published_time" content="2026-10-14T09:58:00Z">
        </head></html>"#;
        let meta = parse_metadata(html, &page).unwrap();
        assert_eq!(meta.title, "Tom & Jerry");
        assert_eq!(meta.description.as_deref(), Some("plain description"));
        assert_eq!(meta.image.as_deref(), Some("https://news.site/img/cover.png"));
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-14T09:58:00Z").unwrap().timestamp_millis();
        assert_eq!(meta.date, Some(expected));

        let plain = parse_metadata(
            r#"<title> Just a title </title><link rel="icon" href="https://cdn.site/i.ico">"#,
            &page,
        )
        .unwrap();
        assert_eq!((plain.title.as_str(), plain.image.as_deref()), ("Just a title", Some("https://cdn.site/i.ico")));
        assert_eq!(parse_metadata("<p>no title</p>", &page), None);
    }
}
