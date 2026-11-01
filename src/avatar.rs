//! Profile and group photos for the thread list.
//!
//! Sources, in order: the contact's Signal profile avatar (fetched with their profile key, cached
//! by presage in `profile_avatars`), the contact photo synced from the phone, the group avatar
//! (`group_avatars`). The picture is cropped square, resampled to the exact physical size and
//! masked to a circle here (the software renderer can't clip images round), then cached as PNG
//! under `thumbs/av-*`.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use presage::libsignal_service::content::GroupContextV2;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::store::{ContentsStore, Thread};
use presage_store_sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::backend::{downscale, Emit, Mgr};
use crate::db::Index;
use crate::types::{Event, Thumb};

/// Logical size of the thread list / header avatar (ui/app.slint `Avatar` default size).
pub const SIZE: u32 = 40;
/// next to incoming group messages (ui/app.slint `Bubble`)
pub const MSG_SIZE: u32 = 28;
/// Emitted key prefix of a message-list avatar: `m:<uuid>`.
pub const MSG_PREFIX: &str = "m:";

/// One photo to load: emitted key, whose photo, logical size.
pub struct Want {
    pub key: String,
    pub thread: Thread,
    pub size: u32,
}
/// Signal profiles (name, photo) are cached forever by presage: refetch them after this.
const PROFILE_REFRESH: Duration = Duration::from_secs(3 * 24 * 3600);
/// pause after a network fetch, so a first run over many threads doesn't hammer the server
const FETCH_GAP: Duration = Duration::from_millis(300);

/// What we last knew about a thread's photo (persisted).
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct State {
    /// contacts: when the profile was last refetched (unix secs)
    #[serde(default)]
    pub checked: u64,
    /// groups: the server path of the avatar we have cached
    #[serde(default)]
    pub group_path: String,
}

pub struct Job {
    pub store: SqliteStore,
    pub index: Index,
    pub manager: Mgr,
    pub emit: Emit,
    pub dir: PathBuf,
    pub state: HashMap<String, State>,
}

/// Result of a job: updated states and whether any profile was refetched (names may have changed).
pub struct Done {
    pub state: HashMap<String, State>,
    pub profiles_refreshed: bool,
}

impl Job {
    /// Loads (fetching when needed) and emits the photos, one after another.
    pub async fn run(mut self, todo: Vec<Want>) -> Done {
        let now = unix_now();
        let mut refreshed = false;
        let mut found = 0;
        for Want { key, thread, size } in todo {
            // state and disk cache belong to the person / group, whatever size is asked for
            let skey = crate::types::thread_key(&thread);
            let mut st = self.state.get(&skey).cloned().unwrap_or_default();
            let (bytes, fetched) = match &thread {
                Thread::Contact(sid) => {
                    let (b, f) = Box::pin(self.contact(sid.raw_uuid(), &mut st, now)).await;
                    refreshed |= f;
                    (b, f)
                }
                Thread::Group(master) => Box::pin(self.group(*master, &mut st)).await,
            };
            self.state.insert(skey.clone(), st);
            found += bytes.is_some() as usize;
            let dir = self.dir.clone();
            let k = key.clone();
            // per item: the UI sets the scale factor shortly after the window appears
            let px = crate::types::physical_px(size);
            let thumb = tokio::task::spawn_blocking(move || bytes.and_then(|b| cached(&dir, &skey, &k, &b, px)))
                .await
                .ok()
                .flatten();
            // an empty thumb clears a photo that went away
            let thumb = thumb.unwrap_or(Thumb { key: key.clone(), width: 0, height: 0, rgba: Vec::new() });
            (self.emit)(Event::Avatar(Arc::new(thumb)));
            if fetched {
                tokio::time::sleep(FETCH_GAP).await;
            }
        }
        debug!(found, refreshed, "avatars loaded");
        Done { state: self.state, profiles_refreshed: refreshed }
    }

    /// (photo bytes, whether the network was used)
    async fn contact(&mut self, uuid: Uuid, st: &mut State, now: u64) -> (Option<Vec<u8>>, bool) {
        let sid = ServiceId::Aci(uuid.into());
        let mut fetched = false;
        if let Ok(Some(key)) = self.store.profile_key(&sid).await {
            let cached = self.store.profile(uuid, key).await.ok().flatten();
            if cached.is_none() || now.saturating_sub(st.checked) > PROFILE_REFRESH.as_secs() {
                fetched = true;
                // presage returns its cached profile as long as there is one
                let _ = self.index.forget_profile(uuid).await;
                match Box::pin(self.manager.retrieve_profile_by_uuid(uuid, key)).await {
                    Ok(p) => {
                        st.checked = now;
                        // also covers a rotated profile key: the photo row is keyed by uuid only
                        if cached.as_ref().map(|c| &c.avatar) != Some(&p.avatar) {
                            let _ = self.index.forget_profile_avatar(uuid).await;
                        }
                    }
                    Err(e) => {
                        debug!("profile {uuid}: {e}");
                        if let Some(old) = cached {
                            let _ = self.store.save_profile(uuid, key, old).await;
                        }
                    }
                }
            }
            if !fetched && self.store.profile_avatar(uuid, key).await.ok().flatten().is_none() {
                // has a profile without a photo, or the photo isn't downloaded yet
                fetched = self.store.profile(uuid, key).await.ok().flatten().is_some_and(|p| p.avatar.is_some());
            }
            match Box::pin(self.manager.retrieve_profile_avatar_by_uuid(uuid, key)).await {
                Ok(Some(b)) => return (Some(b), fetched),
                Ok(None) => {}
                Err(e) => debug!("profile avatar {uuid}: {e}"),
            }
        }
        // the photo saved in the phone's contacts
        let synced = self.store.contact_by_id(&sid).await.ok().flatten().and_then(|c| c.avatar).map(|a| a.reader.to_vec());
        (synced, fetched)
    }

    async fn group(&mut self, master: [u8; 32], st: &mut State) -> (Option<Vec<u8>>, bool) {
        let Ok(Some(g)) = self.store.group(master).await else { return (None, false) };
        if g.avatar.is_empty() {
            st.group_path.clear();
            return (None, false);
        }
        let mut fetched = false;
        if st.group_path != g.avatar {
            // changed (or never seen): presage would keep returning the old cached picture
            let _ = self.index.forget_group_avatar(&master).await;
            fetched = true;
        }
        let ctx = GroupContextV2 { master_key: Some(master.to_vec()), revision: Some(g.revision), ..Default::default() };
        match Box::pin(self.manager.retrieve_group_avatar(ctx)).await {
            Ok(Some(b)) => {
                st.group_path = g.avatar;
                (Some(b), fetched)
            }
            Ok(None) => (None, fetched),
            Err(e) => {
                debug!("group avatar: {e}");
                (None, fetched)
            }
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hash<T: Hash + ?Sized>(v: &T) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// The round photo of `owner` at `px`, from the disk cache when the source bytes haven't changed.
fn cached(dir: &Path, owner: &str, key: &str, bytes: &[u8], px: u32) -> Option<Thumb> {
    let prefix = format!("av-{:016x}-{px}-", hash(owner));
    let name = format!("{prefix}{:016x}.png", hash(bytes));
    let path = dir.join(&name);
    if let Ok(img) = image::open(&path) {
        let rgba = img.to_rgba8();
        return Some(Thumb { key: key.into(), width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() });
    }
    let rgba = match round(bytes, px) {
        Ok(r) => r,
        Err(e) => {
            debug!("avatar {key}: {e}");
            return None;
        }
    };
    // older pictures of this owner at this size, and the pre-size-in-prefix naming
    let legacy = format!("av-{:016x}-", hash(owner));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with(&prefix) || (n.starts_with(&legacy) && n.ends_with(&format!("-{px}.png"))) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let _ = rgba.save(&path);
    Some(Thumb { key: key.into(), width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

/// Center-crops to a square, resamples to `px` and cuts an antialiased circle.
pub fn round(bytes: &[u8], px: u32) -> anyhow::Result<image::RgbaImage> {
    let img = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.decode()?;
    let (w, h) = (img.width(), img.height());
    anyhow::ensure!(w > 0 && h > 0, "empty image");
    let s = w.min(h);
    let square = img.crop_imm((w - s) / 2, (h - s) / 2, s, s).to_rgba8();
    let mut out = downscale(square, px);
    if out.width() != px {
        // smaller than the target: upscale smoothly instead of letting the renderer pixelate it
        out = image::imageops::resize(&out, px, px, image::imageops::FilterType::CatmullRom);
    }
    mask_circle(&mut out);
    Ok(out)
}

fn mask_circle(img: &mut image::RgbaImage) {
    let r = img.width() as f32 / 2.0;
    for (x, y, p) in img.enumerate_pixels_mut() {
        let d = ((x as f32 + 0.5 - r).powi(2) + (y as f32 + 0.5 - r).powi(2)).sqrt();
        let cover = (r - d + 0.5).clamp(0.0, 1.0);
        p[3] = (p[3] as f32 * cover + 0.5) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_crops_square_and_clears_corners() {
        let img = image::RgbaImage::from_pixel(300, 200, image::Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let out = round(&png, 40).unwrap();
        assert_eq!(out.dimensions(), (40, 40));
        assert_eq!(out.get_pixel(0, 0)[3], 0);
        assert_eq!(out.get_pixel(20, 20)[3], 255);
        // small sources are scaled up to the target
        assert_eq!(round(&png, 400).unwrap().dimensions(), (400, 400));
    }
}
