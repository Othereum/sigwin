//! Making a sticker pack: reading `pack.json`, the manifest, and sealing what goes to the CDN.
//! The page is `gui/packmaker.rs`, the upload `backend/packmaker.rs`.

use std::path::{Path, PathBuf};

use presage::libsignal_service::proto::{pack, Pack};
use presage::libsignal_service::sticker_cipher::{derive_key, encrypt_in_place};
use presage::libsignal_service::prelude::ProtobufMessage as _;
use serde::Deserialize;
use sigwin_thumb::sticker::Reject;

/// Signal's limit on stickers in a pack.
pub const MAX_STICKERS: usize = 200;

/// What `pack.json` fills in. Order is the pack's.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub title: String,
    pub author: String,
    /// file and emoji ("" while not chosen)
    pub stickers: Vec<(PathBuf, String)>,
}

#[derive(Deserialize)]
struct RawPack {
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    stickers: Vec<RawSticker>,
}

#[derive(Deserialize)]
struct RawSticker {
    file: Option<String>,
    #[serde(default)]
    emoji: String,
}

/// Reads `pack.json` (`dir`: the folder it is in, which its files are relative to). Keys it
/// doesn't know are ignored. None: it isn't a pack.json.
pub fn parse_pack_json(json: &[u8], dir: &Path) -> Option<Loaded> {
    use unicode_segmentation::UnicodeSegmentation;
    let raw: RawPack = serde_json::from_slice(json).ok()?;
    let stickers = raw
        .stickers
        .into_iter()
        .filter_map(|s| {
            // only files beside it: a pack.json from elsewhere mustn't pull in other pictures
            let file = s.file.filter(|f| !f.trim().is_empty())?;
            if Path::new(&file).components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
                return None;
            }
            let emoji = s.emoji.graphemes(true).next().filter(|g| crate::emoji::is_emoji(g)).unwrap_or_default();
            Some((dir.join(file), emoji.to_string()))
        })
        .collect();
    Some(Loaded { title: raw.title.trim().to_string(), author: raw.author.trim().to_string(), stickers })
}

/// Why a file can't be a sticker, for the user.
pub fn reject_reason(r: Reject) -> String {
    match r {
        Reject::Unreadable => tr!("Not a picture it can read"),
        Reject::Animated => tr!("Only animated PNG (APNG) can move"),
        Reject::NotSquare => tr!("An animated sticker must be square"),
        Reject::TooSmall => tr!("An animated sticker must be at least 10 px"),
        Reject::TooBig => tr!("Too big (at most 512 px for animated stickers)"),
        Reject::MustLoop => tr!("An animated sticker must repeat forever"),
        Reject::TooLarge => tr!("Over 300 KB"),
    }
    .to_string()
}

/// A fresh pack key.
pub fn new_key() -> anyhow::Result<[u8; 32]> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(key)
}

/// The cipher key every file of the pack is sealed with.
pub fn cipher_key(pack_key: &[u8]) -> anyhow::Result<[u8; 64]> {
    derive_key(pack_key).map_err(|e| anyhow::anyhow!("pack key: {e}"))
}

/// IV, AES-CBC and MAC, with a fresh IV per file.
pub fn seal(key: [u8; 64], mut plain: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let mut iv = [0u8; 16];
    getrandom::fill(&mut iv).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    encrypt_in_place(iv, key, &mut plain);
    Ok(plain)
}

/// The manifest: stickers are numbered by their place. `cover`: the id and emoji of the sticker
/// that is the cover (an extra file after the stickers has no emoji).
pub fn manifest(title: &str, author: &str, emoji: &[String], cover: (u32, Option<&str>)) -> Vec<u8> {
    let sticker = |id: u32, e: Option<&str>| pack::Sticker { id: Some(id), emoji: e.filter(|e| !e.is_empty()).map(str::to_string), content_type: None };
    Pack {
        title: Some(title.to_string()),
        author: Some(author.to_string()),
        cover: Some(sticker(cover.0, cover.1)),
        stickers: emoji.iter().enumerate().map(|(i, e)| sticker(i as u32, Some(e))).collect(),
    }
    .encode_to_vec()
}

pub fn link(pack_id: &str, pack_key: &[u8]) -> String {
    format!("https://signal.art/addstickers/#pack_id={pack_id}&pack_key={}", hex::encode(pack_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::sticker_cipher::decrypt_in_place;

    #[test]
    fn a_sealed_manifest_and_sticker_open_with_the_download_side() {
        let key = new_key().unwrap();
        let cipher = cipher_key(&key).unwrap();
        let emoji = vec!["😏".to_string(), "👍".to_string()];
        let mut sealed = seal(cipher, manifest("Cats", "Me", &emoji, (1, Some("👍")))).unwrap();
        decrypt_in_place(cipher_key(&key).unwrap(), &mut sealed).unwrap();
        let back = Pack::decode(sealed.as_slice()).unwrap();
        assert_eq!((back.title(), back.author()), ("Cats", "Me"));
        assert_eq!(back.stickers.iter().map(|s| (s.id(), s.emoji())).collect::<Vec<_>>(), [(0, "😏"), (1, "👍")]);
        let cover = back.cover.unwrap();
        assert_eq!((cover.id(), cover.emoji()), (1, "👍"));

        let picture = vec![7u8; 5000];
        let mut sticker = seal(cipher, picture.clone()).unwrap();
        // sealed twice: a fresh IV each time
        assert_ne!(sticker, seal(cipher, picture.clone()).unwrap());
        decrypt_in_place(cipher, &mut sticker).unwrap();
        assert_eq!(sticker, picture);
        // another pack's key can't open it
        let mut other = seal(cipher, picture).unwrap();
        assert!(decrypt_in_place(cipher_key(&new_key().unwrap()).unwrap(), &mut other).is_err());
    }

    #[test]
    fn an_extra_cover_has_no_emoji_and_the_link_reads_back() {
        let m = Pack::decode(manifest("t", "a", &["😏".into()], (1, None)).as_slice()).unwrap();
        assert_eq!(m.cover.unwrap().emoji, None);
        let l = link("00ff", &[1, 2, 3]);
        assert_eq!(l, "https://signal.art/addstickers/#pack_id=00ff&pack_key=010203");
    }

    #[test]
    fn pack_json_keeps_order_ignores_unknown_keys_and_leaves_emoji_for_later() {
        let json = r#"{"title":" Cats ","author":"Me","extra":1,"stickers":[
            {"file":"001.webp","emoji":"😏","x":1},
            {"file":"sub/002.png"},
            {"file":"003.png","emoji":""},
            {"file":"004.png","emoji":"abc"},
            {"emoji":"👍"},
            {"file":"  "},
            {"file":"../out.png"},
            {"file":"C:\\out.png"}]}"#;
        let l = parse_pack_json(json.as_bytes(), Path::new("dir")).unwrap();
        assert_eq!((l.title.as_str(), l.author.as_str()), ("Cats", "Me"));
        let got: Vec<(PathBuf, &str)> = l.stickers.iter().map(|(p, e)| (p.clone(), e.as_str())).collect();
        assert_eq!(
            got,
            [
                (Path::new("dir").join("001.webp"), "😏"),
                (Path::new("dir").join("sub/002.png"), ""),
                (Path::new("dir").join("003.png"), ""),
                (Path::new("dir").join("004.png"), ""),
            ]
        );
        assert!(parse_pack_json(b"[1,2]", Path::new(".")).is_none());
        assert!(parse_pack_json(b"nope", Path::new(".")).is_none());
        assert_eq!(parse_pack_json(b"{}", Path::new(".")).unwrap(), Loaded::default());
    }
}
