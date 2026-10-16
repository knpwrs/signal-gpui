//! Sticker packs as Signal Desktop's sticker manager shows them
//! (`ts/components/stickers/StickerManager.dom.tsx`,
//! `ts/state/selectors/stickers.std.ts`): installed, the Signal Artist
//! Series, and the packs of stickers we received; and the `signal.art`
//! links that name a pack (`ts/types/Stickers.preload.ts`'s
//! `getDataFromLink`).

use std::path::PathBuf;

use base64::Engine as _;

/// Desktop's `BLESSED_PACKS` (`ts/types/Stickers.preload.ts`): the Signal
/// Artist Series, as hex id and base64 key, in Desktop's order.
const BLESSED_PACKS: [(&str, &str); 11] = [
    ("42fb75e1827c0c945cfb5ca0975db03c", "7uJ+K593PgpV6iTDQLe+hYcRpuK9m27nBEND4OQovmU="),
    ("ccc89a05dc077856b57351e90697976c", "RXMOYPCdVWYRUiN0RTemt9nqmc7qy3eh+9aAG5YH+88="),
    ("fb535407d2f6497ec074df8b9c51dd1d", "F+lxwTQDViJ4HS7iSeZHO3dFg3ULaMEbuCt1CcaLbf0="),
    ("3044281a51307306e5442f2e9070953a", "xMqqhDl+GmMKWWD1SguCdTyIpeUuDe/mFbpN2A8TDL8="),
    ("a2414255948558316f37c1d36c64cd28", "/aEpNxltI28cqeEZalZULh0c72/4TivgOChxf6IK02Y="),
    ("684d2b7bcfc2eec6f57f2e7be0078e0f", "hm4Ny0obJfKwTfJwzXQnI+SmVVwKGrw/PzDcxaIBDFU="),
    ("f19548e5afa38d1ce4f5c3191eba5e30", "LLMHZ0D2aapExsBjKQskmn0ApLAu2PnppbkCo38bvEE="),
    ("9acc9e8aba563d26a4994e69263e3b25", "Wm3/OUjCjvubeq+T7MN1xp/DFueAd+0mhnoU0QoPahI="),
    ("e61fa0867031597467ccc036cc65d403", "E657GnQHMYKA6bOMEmHe044OcTi5+WSmzLtz5A9zeps="),
    ("cca32f5b905208b7d0f1e17f23fdc185", "i/jpX3pFver+DI9bAC7wGrlbjxtbqsQBnM1ra+Cxg3o="),
    ("cfc50156556893ef9838069d3890fe49", "X1vqt9OCRDywCh5I65Upe2uMrf0GMeXQ2dyUnmmZ/0s="),
];

/// The Signal Artist Series packs: `(id, key)`.
pub fn blessed_packs() -> Vec<(Vec<u8>, Vec<u8>)> {
    BLESSED_PACKS
        .iter()
        .filter_map(|(id, key)| Some((hex::decode(id).ok()?, base64::engine::general_purpose::STANDARD.decode(key).ok()?)))
        .collect()
}

pub fn is_blessed(id: &[u8]) -> bool {
    BLESSED_PACKS.iter().any(|(blessed, _)| hex::decode(blessed).is_ok_and(|b| b == id))
}

/// A pack's `signal.art` link (Desktop's `artAddStickersRoute.toWebUrl`).
pub fn pack_link(id: &[u8], key: &[u8]) -> String {
    crate::sticker_upload::share_url(id, key)
}

/// The pack a sticker link names (`signal.art/addstickers` or
/// `sgnl://addstickers`, read by [`crate::routes`]), checked as Desktop's
/// `getDataFromLink` and `isPackIdValid` check it: a 16-byte id and a
/// 32-byte key, in hex.
pub fn parse_pack_link(link: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    match crate::routes::parse(link)? {
        crate::routes::Route::AddStickers { pack_id, pack_key } => pack_of(&pack_id, &pack_key),
        _ => None,
    }
}

/// A route's hex pack id and key, when they're a pack's.
pub fn pack_of(pack_id: &str, pack_key: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let id = hex::decode(pack_id).ok().filter(|i| i.len() == 16)?;
    let key = hex::decode(pack_key).ok().filter(|k| k.len() == 32)?;
    Some((id, key))
}

/// Whether a link is a sticker pack's (Desktop's `isStickerPack`): its
/// preview shows the pack's cover small, and opens the pack.
pub fn is_sticker_pack_link(link: &str) -> bool {
    parse_pack_link(link).is_some()
}

/// One pack as the sticker manager lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct StickerPackSummary {
    pub id: Vec<u8>,
    pub key: Vec<u8>,
    /// Empty until its manifest is fetched.
    pub title: String,
    pub author: String,
    pub cover: Option<PathBuf>,
    pub sticker_count: u32,
    pub installed: bool,
    pub blessed: bool,
}

/// What Desktop's sticker manager lists: "My Stickers" in order, the
/// Signal Artist Series, and "Shared With You" (received packs, installed
/// ones too, newest first).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StickerManagerView {
    pub installed: Vec<StickerPackSummary>,
    pub blessed: Vec<StickerPackSummary>,
    pub received: Vec<StickerPackSummary>,
}

/// A pack opened in Desktop's `StickerPreviewModal`, its stickers
/// downloaded (for one not installed, into a cache).
#[derive(Debug, Clone, PartialEq)]
pub struct StickerPackPreview {
    pub pack: StickerPackSummary,
    pub stickers: Vec<crate::StickerView>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_packs_as_desktops_do() {
        let (id, key) = (vec![0xab; 16], vec![0xcd; 32]);
        let link = pack_link(&id, &key);
        assert_eq!(parse_pack_link(&link), Some((id.clone(), key.clone())));
        assert!(is_sticker_pack_link("https://signal.art/addstickers#pack_id=abababababababababababababababab&pack_key=cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"));
        assert_eq!(parse_pack_link(&link.replace("https://signal.art/addstickers/#", "sgnl://addstickers?")), Some((id.clone(), key.clone())));
        // Another host, a short id, or no key.
        assert_eq!(parse_pack_link(&link.replace("signal.art", "signal.org")), None);
        assert_eq!(parse_pack_link("https://signal.art/addstickers/#pack_id=abab&pack_key=cd"), None);
        assert_eq!(parse_pack_link("https://signal.art/addstickers/#pack_id=abababababababababababababababab"), None);
    }

    #[test]
    fn the_artist_series_is_desktops() {
        let packs = blessed_packs();
        assert_eq!(packs.len(), 11);
        assert!(packs.iter().all(|(id, key)| id.len() == 16 && key.len() == 32));
        assert!(is_blessed(&packs[0].0) && !is_blessed(&[0; 16]));
    }
}
