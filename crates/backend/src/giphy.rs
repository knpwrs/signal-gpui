//! GIF search, following Signal Desktop (`ts/state/smart/fun/giphy.preload.ts`):
//! Giphy's search and trending APIs, rated pg-13, MP4 renditions only, and
//! every request through Signal's content proxy so Giphy doesn't see your
//! IP address. The API key is yours (Settings or `GIPHY_API_KEY`): Signal's
//! own key is for Signal's apps.

use std::time::Duration;

use anyhow::{Context as _, bail};
use serde_json::Value;

const API: &str = "https://api.giphy.com";
/// Signal's CONNECT proxy for third-party content (Desktop's `contentProxyUrl`).
const CONTENT_PROXY: &str = "http://contentproxy.signal.org:443";
const RATING: &str = "pg-13";
const BUNDLE: &str = "messaging_non_clips";
const FIELDS: &str = "id,title,alt_text,images.original.width,images.original.height,images.original.mp4,\
                      images.fixed_width.width,images.fixed_width.height,images.fixed_width.mp4";
pub const PAGE_SIZE: u32 = 30;
/// Giphy's limit on queries, in characters.
const MAX_QUERY_CHARS: usize = 50;
/// GIFs are small; anything bigger isn't one.
const MAX_GIF_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct GifMedia {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gif {
    pub id: String,
    pub title: String,
    /// About 200 wide, for the picker.
    pub preview: GifMedia,
    /// What gets sent.
    pub original: GifMedia,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GifPage {
    pub gifs: Vec<Gif>,
    /// Offset of the next page, if there is one.
    pub next: Option<u32>,
}

/// Desktop only accepts media from Giphy's CDN.
pub fn is_giphy_cdn(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else { return false };
    url.scheme() == "https" && matches!(url.host_str(), Some("media0.giphy.com" | "media1.giphy.com" | "media2.giphy.com" | "media3.giphy.com" | "media4.giphy.com"))
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(crate::link_preview::tls_config()?)
        .proxy(reqwest::Proxy::all(CONTENT_PROXY)?)
        .timeout(Duration::from_secs(30))
        .build()?)
}

/// Search results, or trending GIFs for an empty query.
pub async fn search(api_key: &str, query: &str, offset: u32) -> anyhow::Result<GifPage> {
    let query: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    let (path, mut params) = if query.is_empty() {
        ("v1/gifs/trending", vec![])
    } else {
        ("v1/gifs/search", vec![("q", query)])
    };
    params.extend([
        ("api_key", api_key.to_owned()),
        ("rating", RATING.to_owned()),
        ("bundle", BUNDLE.to_owned()),
        ("fields", FIELDS.to_owned()),
        ("limit", PAGE_SIZE.to_string()),
        ("offset", offset.to_string()),
    ]);
    let url = url::Url::parse_with_params(&format!("{API}/{path}"), &params)?;
    let response = client()?.get(url).send().await.context("reaching Giphy")?;
    let status = response.status();
    let body: Value = response.json().await.context("reading Giphy's answer")?;
    if status.as_u16() == 401 || status.as_u16() == 403 {
        bail!("Giphy rejected the API key");
    }
    if !status.is_success() {
        bail!("Giphy answered {status}");
    }
    parse_page(&body)
}

fn media(value: &Value) -> Option<GifMedia> {
    let number = |v: &Value| v.as_u64().or_else(|| v.as_str()?.parse().ok()).map(|n| n as u32);
    let url = value["mp4"].as_str()?;
    if !is_giphy_cdn(url) {
        return None;
    }
    Some(GifMedia { url: url.to_owned(), width: number(&value["width"])?, height: number(&value["height"])? })
}

fn parse_page(body: &Value) -> anyhow::Result<GifPage> {
    let data = body["data"].as_array().context("Giphy's answer has no results")?;
    let gifs = data
        .iter()
        .filter_map(|item| {
            Some(Gif {
                id: item["id"].as_str()?.to_owned(),
                title: item["title"].as_str().unwrap_or_default().to_owned(),
                preview: media(&item["images"]["fixed_width"])?,
                original: media(&item["images"]["original"])?,
            })
        })
        .collect();
    let pagination = &body["pagination"];
    let (offset, count, total) =
        (pagination["offset"].as_u64(), pagination["count"].as_u64(), pagination["total_count"].as_u64());
    let next = match (offset, count, total) {
        (Some(offset), Some(count), Some(total)) if offset + count < total => Some((offset + count) as u32),
        _ => None,
    };
    Ok(GifPage { gifs, next })
}

/// Downloads a GIF's MP4 (from Giphy's CDN only, through the proxy).
pub async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    if !is_giphy_cdn(url) {
        bail!("not a Giphy CDN URL");
    }
    let response = client()?.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|len| len as usize > MAX_GIF_BYTES) {
        bail!("GIF is too large");
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_GIF_BYTES {
        bail!("GIF is too large");
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_giphy_and_rejects_foreign_media() {
        let body = serde_json::json!({
            "data": [
                {"id": "a", "title": "cat", "images": {
                    "fixed_width": {"mp4": "https://media2.giphy.com/a/200w.mp4", "width": "200", "height": "113"},
                    "original": {"mp4": "https://media2.giphy.com/a/giphy.mp4", "width": "480", "height": "270"}}},
                {"id": "b", "title": "elsewhere", "images": {
                    "fixed_width": {"mp4": "https://evil.example/b.mp4", "width": "200", "height": "200"},
                    "original": {"mp4": "https://media1.giphy.com/b.mp4", "width": "400", "height": "400"}}}
            ],
            "pagination": {"offset": 0, "count": 2, "total_count": 50}
        });
        let page = parse_page(&body).unwrap();
        assert_eq!(page.gifs.len(), 1);
        assert_eq!(page.gifs[0].preview, GifMedia { url: "https://media2.giphy.com/a/200w.mp4".into(), width: 200, height: 113 });
        assert_eq!(page.next, Some(2));
        assert!(!is_giphy_cdn("http://media0.giphy.com/x.mp4"));
    }
}
