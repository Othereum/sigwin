//! Uploading a new sticker pack: the form from the server, the manifest, then the pictures a few
//! at a time. Each picture is read, converted and sealed just before its upload, so what is held
//! is a few files, not the pack.

use std::path::PathBuf;

use futures::StreamExt;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::push_service::{AttachmentV2UploadAttributes, HttpAuthOverride};
use presage::manager::RegistrationData;
use serde::Deserialize;
use sigwin_thumb::sticker::{self, Reject};

use super::*;
use crate::stickerpack;
use crate::types::PackJob;

/// Signal Desktop uploads three at a time too.
const PARALLEL: usize = 3;

/// `GET /v1/sticker/pack/form/{n}`: the pack's id and where each of its `n` files goes.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Form {
    pack_id: String,
    manifest: AttachmentV2UploadAttributes,
    stickers: Vec<AttachmentV2UploadAttributes>,
}

/// Why making the pack stopped.
enum Failure {
    /// this file (its place among the files uploaded; the cover is the last) can't be a sticker
    File(usize, Reject),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(e: E) -> Self {
        Failure::Other(e.into())
    }
}

impl App {
    pub(super) fn create_pack(&mut self, job: PackJob) {
        self.cancel_pack();
        let Some(m) = self.manager.as_ref() else {
            (self.emit)(Event::PackFailed(tr!("Not connected").into()));
            return;
        };
        let reg = m.registration_data().clone();
        let emit = self.emit.clone();
        let index = self.index.clone();
        let task = tokio::task::spawn_local(async move {
            let started = std::time::Instant::now();
            let stickers = job.stickers.len();
            let progress = |done, total| emit(Event::PackProgress { done, total });
            progress(0, stickers + job.cover.is_some() as usize);
            let (title, author) = (job.title.clone(), job.author.clone());
            let cover = job.cover.clone().or_else(|| job.stickers.first().map(|(p, _)| p.clone()));
            match create(&reg, job, &progress).await {
                Ok((id, key)) => {
                    info!(stickers, ms = started.elapsed().as_millis() as u64, "sticker pack uploaded");
                    remember(&index, &id, &key, title, author, cover).await;
                    emit(Event::PackCreated { id, key });
                }
                Err(f) => {
                    // the error text can quote the server's answer: log only what kind it was
                    let http = match &f {
                        Failure::Other(e) => e.downcast_ref::<reqwest::Error>().map(|e| (e.status().map(|s| s.as_u16()), e.is_decode())),
                        Failure::File(..) => None,
                    };
                    let file = matches!(f, Failure::File(..));
                    warn!(stickers, file, status = ?http.and_then(|h| h.0), bad_form = http.is_some_and(|h| h.1), "sticker pack not uploaded");
                    emit(Event::PackFailed(match f {
                        Failure::File(i, r) if i < stickers => tr!("Sticker {}: {}", i + 1, stickerpack::reject_reason(r)),
                        Failure::File(_, r) => tr!("Cover: {}", stickerpack::reject_reason(r)),
                        Failure::Other(e) => format!("{e:#}"),
                    }));
                }
            }
        });
        self.pack_task = Some(task.abort_handle());
    }

    pub(super) fn cancel_pack(&mut self) {
        if let Some(t) = self.pack_task.take() {
            t.abort();
        }
    }
}

/// Keeps the new pack's link on this PC, so it can be found again once the page is left.
async fn remember(index: &Index, id: &str, key: &str, title: String, author: String, cover: Option<PathBuf>) {
    let (Ok(id), Ok(key)) = (hex::decode(id), hex::decode(key)) else { return };
    let cover = match cover {
        Some(path) => tokio::task::spawn_blocking(move || cover_png(&path)).await.ok().flatten(),
        None => None,
    };
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    if let Err(e) = index.add_made_pack(&crate::db::MadePack { id, key, title, author, cover }, at).await {
        warn!("remember made pack: {e}");
    }
}

/// The cover as a still PNG big enough for the pack list at any scale.
fn cover_png(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(sticker::MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    let rgba = sticker::thumb(&bytes, MADE_COVER_PX).ok()?;
    sigwin_thumb::png_rgba(&rgba).ok()
}

/// `PACK_COVER` (32 logical px) at 400%.
const MADE_COVER_PX: u32 = 128;

/// Uploads the pack; its hex id and key.
async fn create(reg: &RegistrationData, job: PackJob, progress: &dyn Fn(usize, usize)) -> Result<(String, String), Failure> {
    // the cover is a file of its own unless it is the first sticker
    let mut files: Vec<PathBuf> = job.stickers.iter().map(|(p, _)| p.clone()).collect();
    let cover_id = match &job.cover {
        Some(c) => {
            files.push(c.clone());
            job.stickers.len() as u32
        }
        None => 0,
    };
    let total = files.len();
    let service = super::storage::push_service(reg)?;
    let form: Form = service
        .request(reqwest::Method::GET, Endpoint::service(format!("/v1/sticker/pack/form/{total}")), HttpAuthOverride::NoOverride)?
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?
        .json()
        .await
        .map_err(reqwest::Error::without_url)?;
    if form.stickers.len() != total {
        return Err(anyhow::anyhow!("the server made room for {} files, not {total}", form.stickers.len()).into());
    }

    let pack_key = stickerpack::new_key()?;
    let cipher = stickerpack::cipher_key(&pack_key)?;
    let emoji: Vec<String> = job.stickers.iter().map(|(_, e)| e.clone()).collect();
    let cover_emoji = job.cover.is_none().then(|| emoji.first().map(String::as_str)).flatten();
    let manifest = stickerpack::seal(cipher, stickerpack::manifest(&job.title, &job.author, &emoji, (cover_id, cover_emoji)))?;
    service.clone().upload_to_cdn0("/", form.manifest, "manifest".into(), &manifest[..]).await?;
    drop(manifest);

    let uploads = files.into_iter().zip(form.stickers).enumerate().map(|(i, (path, attrs))| {
        let mut service = service.clone();
        async move {
            let bytes = tokio::task::spawn_blocking(move || read_and_seal(&path, cipher)).await.map_err(anyhow::Error::from)?.map_err(|r| Failure::File(i, r))?;
            service.upload_to_cdn0("/", attrs, "file".into(), &bytes[..]).await?;
            Ok::<(), Failure>(())
        }
    });
    let mut running = futures::stream::iter(uploads).buffer_unordered(PARALLEL);
    let mut done = 0;
    while let Some(r) = running.next().await {
        r?;
        done += 1;
        progress(done, total);
    }
    Ok((form.pack_id, hex::encode(pack_key)))
}

/// The file as it will be uploaded: converted if it must be, then sealed.
fn read_and_seal(path: &std::path::Path, cipher: [u8; 64]) -> Result<Vec<u8>, Reject> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| Reject::Unreadable)?;
    let mut bytes = Vec::new();
    file.take(sticker::MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes).map_err(|_| Reject::Unreadable)?;
    if bytes.len() as u64 > sticker::MAX_SOURCE_BYTES {
        return Err(Reject::TooLarge);
    }
    let ready = sticker::prepare(bytes)?;
    stickerpack::seal(cipher, ready).map_err(|_| Reject::Unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::sticker_cipher::decrypt_in_place;

    #[test]
    fn a_file_is_read_converted_sealed_and_opens_again() {
        let dir = std::env::temp_dir().join(format!("sigwin-packmaker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wide = image::RgbaImage::from_fn(700, 300, |x, y| image::Rgba([(x / 3) as u8, (y / 2) as u8, 50, 255]));
        let png = sigwin_thumb::png_rgba(&wide).unwrap();
        std::fs::write(dir.join("a.png"), &png).unwrap();
        std::fs::write(dir.join("b.txt"), b"hello").unwrap();

        let key = stickerpack::new_key().unwrap();
        let cipher = stickerpack::cipher_key(&key).unwrap();
        let mut sealed = read_and_seal(&dir.join("a.png"), cipher).unwrap();
        decrypt_in_place(cipher, &mut sealed).unwrap();
        // converted: a 512x512 canvas, not the 700x300 source
        assert_eq!(sigwin_thumb::decode(&sealed).unwrap().to_rgba8().dimensions(), (512, 512));
        assert_eq!(read_and_seal(&dir.join("b.txt"), cipher), Err(Reject::Unreadable));
        assert_eq!(read_and_seal(&dir.join("missing.png"), cipher), Err(Reject::Unreadable));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
