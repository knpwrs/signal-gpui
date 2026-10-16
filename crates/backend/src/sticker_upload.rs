//! Uploading a new sticker pack, as Signal Desktop's sticker pack creator
//! does (sticker-creator/src/util/crypto.ts, ts/textsecure/WebAPI
//! `putStickers`): a random pack key, every file encrypted with keys derived
//! from it, an upload form from the chat server, then each file posted to
//! CDN 0. The server picks the pack id.

use futures::StreamExt as _;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::prelude::ProtobufMessage as _;
use presage::libsignal_service::push_service::{AttachmentV2UploadAttributes, HttpAuthOverride, PushService};
use presage::libsignal_service::sticker_cipher::{derive_key, encrypt_in_place};
use presage::proto::{Pack, pack};
use rand::Rng as _;

use crate::actor::SignalManager;

/// Desktop's `MAX_STICKERS`.
pub const MAX_STICKERS: usize = 200;
/// Desktop's `MAX_STICKER_BYTE_SIZE`, for each processed sticker.
pub const MAX_STICKER_BYTES: usize = 300 * 1024;
/// Stickers posted to the CDN at once (Desktop's upload queue).
const CONCURRENT_UPLOADS: usize = 3;

/// A pack to upload, its stickers in order, each already processed (512px
/// WebP, or an animated PNG as given).
#[derive(Debug, Clone, PartialEq)]
pub struct NewStickerPack {
    pub title: String,
    pub author: String,
    pub stickers: Vec<NewSticker>,
    pub cover: Cover,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSticker {
    pub image: StickerImage,
    pub emoji: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickerImage {
    pub bytes: Vec<u8>,
    /// `image/webp`, or `image/png` for an animated PNG.
    pub content_type: &'static str,
}

/// The pack's cover: one of its stickers (by index), or an image of its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Cover {
    Sticker(usize),
    Image(StickerImage),
}

/// How an upload is going, answered to `Command::UploadStickerPack`.
#[derive(Debug, Clone, PartialEq)]
pub enum StickerUpload {
    /// `done` of `total` stickers are on the CDN.
    Progress { done: usize, total: usize },
    /// Uploaded and installed; `url` is the pack's signal.art link.
    Done { pack_id: Vec<u8>, pack_key: Vec<u8>, url: String },
    Failed(String),
}

/// The link Desktop shares for a pack (selectors/art.ts).
pub fn share_url(pack_id: &[u8], pack_key: &[u8]) -> String {
    format!("https://signal.art/addstickers/#pack_id={}&pack_key={}", hex::encode(pack_id), hex::encode(pack_key))
}

/// The manifest and the files to upload, in upload order: sticker `i` is
/// file `i`, and a cover that isn't one of the stickers comes last.
pub(crate) fn plaintext(pack: &NewStickerPack) -> (Pack, Vec<&StickerImage>) {
    let sticker = |id: usize, emoji: Option<&str>, image: &StickerImage| pack::Sticker {
        id: Some(id as u32),
        emoji: emoji.map(str::to_owned),
        content_type: Some(image.content_type.to_owned()),
    };
    let mut files: Vec<&StickerImage> = pack.stickers.iter().map(|s| &s.image).collect();
    let stickers: Vec<pack::Sticker> =
        pack.stickers.iter().enumerate().map(|(id, s)| sticker(id, Some(&s.emoji), &s.image)).collect();
    // Desktop: a cover with the same bytes as a sticker is that sticker.
    let cover = match &pack.cover {
        Cover::Sticker(ix) => stickers.get(*ix).cloned(),
        Cover::Image(image) => match pack.stickers.iter().position(|s| s.image.bytes == image.bytes) {
            Some(ix) => Some(stickers[ix].clone()),
            None => {
                files.push(image);
                Some(sticker(files.len() - 1, None, image))
            }
        },
    };
    let manifest = Pack { title: Some(pack.title.clone()), author: Some(pack.author.clone()), cover, stickers };
    (manifest, files)
}

/// Encrypts a file with the pack's keys: IV, AES-256-CBC, HMAC-SHA256.
pub(crate) fn encrypt(key: [u8; 64], plaintext: &[u8]) -> Vec<u8> {
    let mut buffer = plaintext.to_vec();
    encrypt_in_place(rand::rng().random(), key, &mut buffer);
    buffer
}

/// `GET /v1/sticker/pack/form/{count}`: `{packId, manifest, stickers}`,
/// with an upload form for the manifest and for each file.
struct UploadForm {
    pack_id: String,
    manifest: AttachmentV2UploadAttributes,
    stickers: Vec<AttachmentV2UploadAttributes>,
}

impl UploadForm {
    fn parse(json: &[u8]) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let mut json: serde_json::Value = serde_json::from_slice(json)?;
        let pack_id = json["packId"].as_str().context("no packId")?.to_owned();
        Ok(Self {
            pack_id,
            manifest: serde_json::from_value(json["manifest"].take())?,
            stickers: serde_json::from_value(json["stickers"].take())?,
        })
    }
}

/// Uploads and installs `pack`, reporting each sticker uploaded. Returns
/// the pack's id and key.
pub(crate) async fn upload(
    manager: &mut SignalManager,
    pack: &NewStickerPack,
    progress: impl Fn(usize, usize),
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    anyhow::ensure!(!pack.stickers.is_empty() && pack.stickers.len() <= MAX_STICKERS, "a pack has 1 to 200 stickers");
    let pack_key: [u8; 32] = rand::rng().random();
    let key = derive_key(&pack_key)?;
    let (manifest, files) = plaintext(pack);
    let manifest = encrypt(key, &manifest.encode_to_vec());
    let files: Vec<Vec<u8>> = files.iter().map(|f| encrypt(key, &f.bytes)).collect();

    let service = crate::storage::push_service(manager)?;
    let path = format!("/v1/sticker/pack/form/{}", files.len());
    let response = service
        .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
        .send()
        .await?
        .error_for_status()?;
    let form = UploadForm::parse(&response.bytes().await?)?;
    anyhow::ensure!(form.stickers.len() == files.len(), "the server sent {} upload forms for {} files", form.stickers.len(), files.len());
    let pack_id = hex::decode(&form.pack_id)?;

    post(&service, form.manifest, manifest).await?;
    let total = pack.stickers.len();
    let mut done = 0;
    let mut uploads = futures::stream::iter(form.stickers.into_iter().zip(files))
        .map(|(attributes, file)| post(&service, attributes, file))
        .buffer_unordered(CONCURRENT_UPLOADS);
    while let Some(result) = uploads.next().await {
        result?;
        // A separate cover isn't one of the stickers counted.
        done = (done + 1).min(total);
        progress(done, total);
    }

    // Desktop downloads the pack back to install it, and tells the other
    // devices (ts/types/Stickers `installStickerPack`, actionSource 'ui').
    // It doesn't wait for that (sticker-creator/src/util/api.ts), so a
    // failed install doesn't fail the upload: the pack is on the CDN and
    // its link works, and saying it failed would only invite uploading it
    // twice.
    if let Err(error) = manager.install_sticker_pack(&pack_id, &pack_key).await {
        tracing::warn!(%error, "uploaded a sticker pack but couldn't install it");
    }
    Ok((pack_id, pack_key.to_vec()))
}

async fn post(service: &PushService, attributes: AttachmentV2UploadAttributes, file: Vec<u8>) -> anyhow::Result<()> {
    let mut service = service.clone();
    service.upload_to_cdn0("/", attributes, "file".into(), std::io::Cursor::new(file)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use presage::libsignal_service::sticker_cipher::decrypt_in_place;

    use super::*;

    fn image(byte: u8) -> StickerImage {
        StickerImage { bytes: vec![byte; 8], content_type: "image/webp" }
    }

    fn pack(cover: Cover) -> NewStickerPack {
        NewStickerPack {
            title: "Hikes".into(),
            author: "Me".into(),
            stickers: vec![
                NewSticker { image: image(1), emoji: "🥾".into() },
                NewSticker { image: image(2), emoji: "⛰️".into() },
            ],
            cover,
        }
    }

    #[test]
    fn stickers_keep_their_order_and_emoji() {
        let new = pack(Cover::Sticker(1));
        let (manifest, files) = plaintext(&new);
        assert_eq!(files.len(), 2);
        let ids: Vec<_> = manifest.stickers.iter().map(|s| (s.id(), s.emoji())).collect();
        assert_eq!(ids, [(0, "🥾"), (1, "⛰️")]);
        assert_eq!(manifest.cover.as_ref().map(|c| c.id()), Some(1));
        assert_eq!((manifest.title(), manifest.author()), ("Hikes", "Me"));
    }

    #[test]
    fn a_cover_of_its_own_is_uploaded_last() {
        let new = pack(Cover::Image(image(9)));
        let (manifest, files) = plaintext(&new);
        assert_eq!(files.len(), 3);
        assert_eq!(files[2].bytes, image(9).bytes);
        let cover = manifest.cover.unwrap();
        assert_eq!((cover.id(), cover.emoji.as_deref()), (2, None));
        assert_eq!(manifest.stickers.len(), 2, "the cover isn't a sticker");

        // The same image as a sticker is that sticker.
        let new = pack(Cover::Image(image(2)));
        let (manifest, files) = plaintext(&new);
        assert_eq!((files.len(), manifest.cover.unwrap().id()), (2, 1));
    }

    #[test]
    fn files_decrypt_with_the_derived_key() {
        let key = derive_key(&[7; 32]).unwrap();
        let mut sealed = encrypt(key, b"sticker bytes");
        assert_ne!(&sealed[16..], b"sticker bytes");
        decrypt_in_place(key, &mut sealed).unwrap();
        assert_eq!(sealed, b"sticker bytes");
    }

    #[test]
    fn the_upload_form_parses() {
        let attrs = r#"{"key":"k","credential":"c","acl":"a","algorithm":"x","date":"d","policy":"p","signature":"s"}"#;
        let json = format!(r#"{{"packId":"ab01","manifest":{attrs},"stickers":[{attrs},{attrs}]}}"#);
        let form = UploadForm::parse(json.as_bytes()).unwrap();
        assert_eq!((form.pack_id.as_str(), form.stickers.len()), ("ab01", 2));
        assert!(UploadForm::parse(b"{}").is_err());
    }

    #[test]
    fn share_links_carry_hex_id_and_key() {
        assert_eq!(
            share_url(&[0xab, 0x01], &[0xff; 2]),
            "https://signal.art/addstickers/#pack_id=ab01&pack_key=ffff"
        );
    }
}
