//! Attachment uploads to Signal's CDN 3, streamed from the file being sent.
//!
//! libsignal-service's own upload holds the whole file in memory several times over (padded,
//! encrypted, then copied again for the request), and its client drops any request that hears
//! nothing for 65s: an upload hears nothing until the server has the whole body, so every upload
//! taking longer than that failed. Like Signal Desktop, this sends one PATCH from a stream and,
//! when it breaks, asks the server how much it got and goes on from there.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use aes::cipher::consts::{U16, U32, U64};
use aes::cipher::{Array, BlockModeEncrypt, KeyIvInit};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Url;
use presage::libsignal_service::configuration::{Endpoint, ServiceConfiguration};
use presage::libsignal_service::push_service::{AttachmentUploadForm, HttpAuthOverride};
use presage::manager::RegistrationData;
use presage::proto::attachment_pointer::AttachmentIdentifier;
use sha2::Digest;

use super::*;

/// What gets uploaded: bytes already in memory (a prepared photo) or a file read as it goes.
pub(super) enum Source {
    Bytes(Vec<u8>),
    File { path: PathBuf, len: u64 },
}

impl Source {
    pub(super) fn len(&self) -> u64 {
        match self {
            Source::Bytes(b) => b.len() as u64,
            Source::File { len, .. } => *len,
        }
    }

    /// The plain bytes, from the start.
    fn reader(&self) -> anyhow::Result<Box<dyn Read + Send + '_>> {
        Ok(match self {
            Source::Bytes(b) => Box::new(b.as_slice()),
            Source::File { path, .. } => crate::attached::open(path).context("open file to send")?,
        })
    }

    /// The whole of it in memory, for the CDN this doesn't stream to.
    pub(super) fn into_bytes(self) -> anyhow::Result<Vec<u8>> {
        match self {
            Source::Bytes(b) => Ok(b),
            Source::File { path, .. } => crate::attached::read(&path).context("read file to send"),
        }
    }
}

/// How long an upload may go without moving before it's dropped and resumed.
const STALL: Duration = Duration::from_secs(60);
/// Tries of one upload; each resumes where the last one stopped.
const TRIES: u32 = 5;
/// The size of what the reading thread hands over at a time, and how many it may be ahead.
const CHUNK: usize = 256 * 1024;
const AHEAD: usize = 4;

/// The upload form, if the server offers CDN 3 for this one. None: another CDN, which the
/// caller leaves to presage.
pub(super) async fn form(reg: &RegistrationData, len: u64) -> anyhow::Result<Option<AttachmentUploadForm>> {
    let service = super::storage::push_service(reg)?;
    let path = format!("/v4/attachments/form/upload?uploadLength={}", encrypted_len(len));
    let form: AttachmentUploadForm = service
        .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)?
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?
        .json()
        .await
        .map_err(reqwest::Error::without_url)?;
    Ok((form.cdn == 3).then_some(form))
}

/// Encrypts `src` and uploads it on CDN 3 with `form`. `progress` gets the bytes the server has
/// so far, as best known (what the request has taken).
pub(super) async fn upload(form: AttachmentUploadForm, spec: AttachmentSpec, src: Source, progress: &dyn Fn(u64)) -> anyhow::Result<AttachmentPointer> {
    let mut key = [0u8; 64];
    let mut iv = [0u8; 16];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    getrandom::fill(&mut iv).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let len = src.len();
    let total = encrypted_len(len);
    let cfg = ServiceConfiguration::from(SignalServers::Production);
    let client = client(&cfg)?;
    let src = Arc::new(src);

    let mut req = client.post(form.signed_upload_location.clone());
    for (k, v) in &form.headers {
        req = req.header(k, v);
    }
    let res = req
        .header(reqwest::header::CONTENT_LENGTH, "0")
        .header("Upload-Defer-Length", "1")
        .header("Tus-Resumable", "1.0.0")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    let location: Url = res.headers().get(reqwest::header::LOCATION).context("no upload location")?.to_str()?.parse()?;

    let mut offset = 0;
    let mut tries = 0;
    let digest = loop {
        tries += 1;
        let (body, digest) = stream(src.clone(), key, iv, offset);
        let res = if offset == total {
            // the server has it all already: only the digest is missing
            drain(body).await
        } else {
            patch(&client, &cfg, &form, &location, offset, total, body, progress).await
        };
        let mut last = match res {
            Ok(()) => break digest.await.context("encrypting the file stopped")??,
            Err(e) if e.retry => {
                warn!(tries, from = offset, total, "upload broke off: {:#}", e.error);
                e.error
            }
            Err(e) => return Err(e.error),
        };
        // the next try starts where the server says it got to, never from a guess: bytes sent
        // from anywhere else are refused (409)
        offset = loop {
            if tries >= TRIES {
                return Err(last.context(format!("upload incomplete after {tries} tries")));
            }
            tokio::time::sleep(Duration::from_secs(1 << tries)).await;
            match resume_offset(&client, &cfg, &form, &location).await {
                Ok(o) if o <= total => break o,
                Ok(_) => anyhow::bail!("the server has more of the upload than was sent"),
                Err(e) => {
                    warn!(tries, "upload offset: {e:#}");
                    last = e;
                    tries += 1;
                }
            }
        };
    };
    info!(bytes = total, tries, "uploaded");
    let flags = if spec.voice_note == Some(true) { presage::proto::attachment_pointer::Flags::VoiceMessage as u32 } else { 0 }
        | if spec.borderless == Some(true) { presage::proto::attachment_pointer::Flags::Borderless as u32 } else { 0 };
    Ok(AttachmentPointer {
        content_type: Some(spec.content_type),
        key: Some(key.to_vec()),
        size: Some(len as u32),
        digest: Some(digest),
        file_name: spec.file_name,
        flags: Some(flags),
        width: spec.width,
        height: spec.height,
        caption: spec.caption,
        blur_hash: spec.blur_hash,
        upload_timestamp: Some(view::time::now_ms()),
        cdn_number: Some(form.cdn),
        attachment_identifier: Some(AttachmentIdentifier::CdnKey(form.key)),
        ..Default::default()
    })
}

/// The CDN client: the Signal certificates and user agent presage uses, and no read timeout,
/// which a long upload would hit. A stalled upload is caught by `patch` instead.
fn client(cfg: &ServiceConfiguration) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::ClientBuilder::new()
        .tls_certs_only([reqwest::Certificate::from_pem(cfg.certificate_authority.as_bytes())?])
        .connect_timeout(Duration::from_secs(10))
        .user_agent(super::files::PRESAGE_USER_AGENT)
        .http1_only()
        .build()?)
}

struct Failed {
    error: anyhow::Error,
    /// worth resuming: the connection broke or the server had trouble, not a refusal
    retry: bool,
}

impl Failed {
    fn from_reqwest(e: reqwest::Error) -> Self {
        // 409: the server's offset isn't the one sent from; asking it again sorts that out
        let retry = e.status().is_none_or(|s| s.is_server_error() || s == reqwest::StatusCode::CONFLICT);
        Failed { error: e.without_url().into(), retry }
    }
}

/// Sends the bytes from `offset` on; dropped when nothing moves for `STALL`.
#[allow(clippy::too_many_arguments)]
async fn patch(client: &reqwest::Client, cfg: &ServiceConfiguration, form: &AttachmentUploadForm, location: &Url, offset: u64, total: u64, body: Body, progress: &dyn Fn(u64)) -> Result<(), Failed> {
    let url = Endpoint::cdn(3, location.path()).into_url(cfg).map_err(|e| Failed { error: e.into(), retry: false })?;
    let mut req = client.patch(url);
    for (k, v) in &form.headers {
        req = req.header(k, v);
    }
    let (moved, taken) = (body.moved.clone(), body.taken.clone());
    let send = req
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Offset", offset)
        .header("Upload-Length", total)
        .header(reqwest::header::CONTENT_TYPE, "application/offset+octet-stream")
        .header(reqwest::header::CONTENT_LENGTH, total - offset)
        .body(reqwest::Body::wrap_stream(body.chunks))
        .send();
    let started = Instant::now();
    let stalled = async {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            progress(offset + taken.load(Ordering::Relaxed));
            let last = Duration::from_millis(moved.load(Ordering::Relaxed));
            if started.elapsed().saturating_sub(last) > STALL {
                return;
            }
        }
    };
    tokio::select! {
        res = send => res.and_then(reqwest::Response::error_for_status).map(drop).map_err(Failed::from_reqwest),
        () = stalled => Err(Failed { error: anyhow::anyhow!("no progress for {}s", STALL.as_secs()), retry: true }),
    }
}

/// How much of the upload the server has.
async fn resume_offset(client: &reqwest::Client, cfg: &ServiceConfiguration, form: &AttachmentUploadForm, location: &Url) -> anyhow::Result<u64> {
    let mut req = client.head(Endpoint::cdn_url(3, location).into_url(cfg)?).header("Tus-Resumable", "1.0.0");
    for (k, v) in &form.headers {
        req = req.header(k, v);
    }
    let res = req
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    Ok(res.headers().get("upload-offset").context("no Upload-Offset")?.to_str()?.parse()?)
}

/// The encrypted upload, as the request reads it.
struct Body {
    chunks: futures::stream::BoxStream<'static, std::io::Result<Vec<u8>>>,
    /// ms after the request started that the last chunk was taken: how `patch` sees progress
    moved: Arc<AtomicU64>,
    /// bytes the request has taken
    taken: Arc<AtomicU64>,
}

async fn drain(mut body: Body) -> Result<(), Failed> {
    use futures::StreamExt;
    while let Some(c) = body.chunks.next().await {
        c.map_err(|e| Failed { error: e.into(), retry: false })?;
    }
    Ok(())
}

/// Encrypts `src` on a thread of its own, a few chunks ahead of the request, and hands over the
/// bytes from `skip` on. The whole is encrypted every time, since CBC can't start in the middle,
/// and so the digest of the whole comes out of every run.
fn stream(src: Arc<Source>, key: [u8; 64], iv: [u8; 16], skip: u64) -> (Body, oneshot::Receiver<anyhow::Result<Vec<u8>>>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(AHEAD);
    let (digest_tx, digest_rx) = oneshot::channel();
    std::thread::spawn(move || {
        let mut sent = 0u64;
        let res = encrypt(&src, key, iv, |out| {
            let end = sent + out.len() as u64;
            if end > skip {
                let from = skip.saturating_sub(sent) as usize;
                tx.blocking_send(Ok(out[from..].to_vec())).map_err(|_| anyhow::anyhow!("upload dropped"))?;
            }
            sent = end;
            Ok(())
        });
        if let Err(e) = &res {
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("{e:#}"))));
        }
        let _ = digest_tx.send(res);
    });
    let moved = Arc::new(AtomicU64::new(0));
    let taken = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let (seen, counted) = (moved.clone(), taken.clone());
    let chunks = futures::stream::unfold(rx, move |mut rx| {
        let (seen, counted) = (seen.clone(), counted.clone());
        async move {
            let c = rx.recv().await?;
            seen.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            if let Ok(c) = &c {
                counted.fetch_add(c.len() as u64, Ordering::Relaxed);
            }
            Some((c, rx))
        }
    });
    (Body { chunks: Box::pin(chunks), moved, taken }, digest_rx)
}

/// Signal's attachment encryption of `src`, given to `out` a chunk at a time: the IV, the plain
/// bytes padded with zeros to `padded_len` then PKCS#7, AES-256-CBC, and an HMAC-SHA256 of all
/// before it. Returns the SHA-256 of everything given out (the pointer's digest).
fn encrypt(src: &Source, key: [u8; 64], iv: [u8; 16], mut out: impl FnMut(&[u8]) -> anyhow::Result<()>) -> anyhow::Result<Vec<u8>> {
    let len = src.len();
    let padded = padded_len(len);
    let key: &Array<u8, U64> = (&key).into();
    let (aes_key, mac_key) = key.split_ref::<U32>();
    let mut aes = cbc::Encryptor::<aes::Aes256>::new(aes_key, &iv.into());
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(mac_key).expect("fixed length key");
    let mut digest = sha2::Sha256::new();
    let mut give = |b: &[u8], mac: &mut Hmac<sha2::Sha256>| -> anyhow::Result<()> {
        mac.update(b);
        digest.update(b);
        out(b)
    };
    give(&iv, &mut mac)?;

    let mut reader = src.reader()?;
    // room for the PKCS#7 bytes after the last chunk
    let mut buf = vec![0u8; CHUNK + 16];
    // of the plain bytes padded with zeros
    let mut pos = 0u64;
    loop {
        let mut n = 0;
        while n < CHUNK && pos < padded {
            let want = (CHUNK - n).min((padded - pos) as usize);
            let got = if pos < len {
                let got = reader.read(&mut buf[n..n + want.min((len - pos) as usize)])?;
                anyhow::ensure!(got > 0, "the file got shorter while it was being sent");
                got
            } else {
                buf[n..n + want].fill(0);
                want
            };
            n += got;
            pos += got as u64;
        }
        // every chunk but the last is whole blocks (CHUNK is), the last gets PKCS#7
        let last = pos == padded;
        if last {
            let pad = 16 - n % 16;
            buf[n..n + pad].fill(pad as u8);
            n += pad;
        }
        let (blocks, _) = Array::<u8, U16>::slice_as_chunks_mut(&mut buf[..n]);
        aes.encrypt_blocks(blocks);
        give(&buf[..n], &mut mac)?;
        if last {
            break;
        }
    }
    let tag = mac.finalize().into_bytes();
    digest.update(&tag[..]);
    out(&tag[..])?;
    Ok(digest.finalize().to_vec())
}

/// Signal's padding: up to the next power of 1.05, so sizes give less away.
fn padded_len(len: u64) -> u64 {
    let p = 1.05f64.powf((len as f64).log(1.05).ceil()).floor() as u64;
    p.max(541).max(len)
}

/// IV, the padded plain bytes plus PKCS#7 (always at least one byte), MAC.
fn encrypted_len(len: u64) -> u64 {
    16 + (padded_len(len) / 16 + 1) * 16 + 32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What libsignal-service uploads for the same key and IV.
    fn reference(plain: &[u8], key: [u8; 64], iv: [u8; 16]) -> Vec<u8> {
        let mut data = plain.to_vec();
        data.resize(padded_len(plain.len() as u64) as usize, 0);
        presage::libsignal_service::attachment_cipher::encrypt_in_place(iv, key, &mut data);
        data
    }

    #[test]
    fn the_stream_is_what_libsignal_service_would_upload() {
        let key: [u8; 64] = std::array::from_fn(|i| i as u8);
        let iv: [u8; 16] = std::array::from_fn(|i| 200 - i as u8);
        // around the chunk size and block edges, and sizes padding takes to a block's end
        for n in [0, 1, 15, 16, 541, 542, 1000, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17, 1_234_567] {
            let plain: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            let want = reference(&plain, key, iv);
            let mut got = Vec::new();
            let digest = encrypt(&Source::Bytes(plain), key, iv, |b| {
                got.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            assert_eq!(got, want, "{n} bytes");
            assert_eq!(got.len() as u64, encrypted_len(n as u64), "{n} bytes");
            assert_eq!(digest, sha2::Sha256::digest(&want).to_vec(), "{n} bytes");
        }
    }

    #[test]
    fn a_resumed_stream_starts_where_the_server_stopped() {
        let key = [3u8; 64];
        let iv = [9u8; 16];
        let plain: Vec<u8> = (0..CHUNK * 2 + 100).map(|i| i as u8).collect();
        let want = reference(&plain, key, iv);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        for skip in [0, 1, 16, CHUNK as u64 + 5, want.len() as u64 - 1, want.len() as u64] {
            let (mut body, digest) = stream(Arc::new(Source::Bytes(plain.clone())), key, iv, skip);
            let got = rt.block_on(async {
                use futures::StreamExt;
                let mut got = Vec::new();
                while let Some(c) = body.chunks.next().await {
                    got.extend_from_slice(&c.unwrap());
                }
                got
            });
            assert_eq!(got, want[skip as usize..], "skip {skip}");
            assert_eq!(rt.block_on(digest).unwrap().unwrap(), sha2::Sha256::digest(&want).to_vec());
        }
    }

    #[test]
    fn a_file_streams_like_its_bytes() {
        let dir = std::env::temp_dir().join(format!("sigwin-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        let plain: Vec<u8> = (0..CHUNK + 33).map(|i| (i % 13) as u8).collect();
        std::fs::write(&path, &plain).unwrap();
        let (key, iv) = ([1u8; 64], [2u8; 16]);
        let mut got = Vec::new();
        encrypt(&Source::File { path, len: plain.len() as u64 }, key, iv, |b| {
            got.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, reference(&plain, key, iv));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
