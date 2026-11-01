//! Encrypted cache files (downloaded attachments, thumbnails, avatars), like Signal Desktop's
//! local attachment encryption: copied off the disk they are useless without the DB key, which
//! is DPAPI-protected.
//!
//! Format: `SWV1` | 16-byte salt | chunks. The file key is HKDF-SHA256(root, salt), so every
//! file has its own key. Each chunk is up to 64 KiB of plaintext sealed with AES-256-GCM plus
//! its 16-byte tag; the nonce is the chunk index and a last-chunk flag, so reordered, dropped or
//! truncated chunks fail to open. The last chunk is always shorter than 64 KiB (maybe empty).

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aes_gcm::aead::{AeadInOut, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;

const MAGIC: &[u8; 4] = b"SWV1";
const SALT: usize = 16;
const CHUNK: usize = 64 * 1024;
const TAG: usize = 16;

static ROOT: OnceLock<[u8; 32]> = OnceLock::new();

/// Sets the root key (once per process; later calls are ignored).
pub fn init(root: [u8; 32]) {
    let _ = ROOT.set(root);
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn cipher(salt: &[u8]) -> io::Result<Aes256Gcm> {
    let root = ROOT.get().ok_or_else(|| io::Error::other("vault key not set"))?;
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), root).expand(b"sigwin file v1", &mut key).map_err(|_| io::Error::other("hkdf"))?;
    Aes256Gcm::new_from_slice(&key).map_err(|_| io::Error::other("key length"))
}

fn nonce(i: u64, last: bool) -> Nonce<aes_gcm::aead::consts::U12> {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&i.to_be_bytes());
    n[11] = last as u8;
    n.into()
}

/// Reads until `buf` is full or the input ends.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

pub fn encrypt(mut r: impl Read, mut w: impl Write) -> io::Result<()> {
    let mut salt = [0u8; SALT];
    getrandom::fill(&mut salt).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;
    let c = cipher(&salt)?;
    w.write_all(MAGIC)?;
    w.write_all(&salt)?;
    let mut buf = vec![0u8; CHUNK];
    let mut i = 0u64;
    loop {
        let n = fill(&mut r, &mut buf)?;
        let last = n < CHUNK;
        let tag = c
            .encrypt_inout_detached(&nonce(i, last), MAGIC, (&mut buf[..n]).into())
            .map_err(|_| io::Error::other("encrypt"))?;
        w.write_all(&buf[..n])?;
        w.write_all(&tag)?;
        if last {
            return w.flush();
        }
        i += 1;
    }
}

pub fn decrypt(mut r: impl Read, mut w: impl Write) -> io::Result<()> {
    let mut head = [0u8; MAGIC.len() + SALT];
    if fill(&mut r, &mut head)? != head.len() || &head[..MAGIC.len()] != MAGIC {
        return Err(bad("not a vault file"));
    }
    let c = cipher(&head[MAGIC.len()..])?;
    let mut buf = vec![0u8; CHUNK + TAG];
    let mut i = 0u64;
    loop {
        let n = fill(&mut r, &mut buf)?;
        if n < TAG {
            return Err(bad("truncated vault file"));
        }
        let last = n < CHUNK + TAG;
        let (data, tag) = buf[..n].split_at_mut(n - TAG);
        let tag = Tag::<aes_gcm::aead::consts::U16>::try_from(&*tag).map_err(|_| bad("tag"))?;
        c.decrypt_inout_detached(&nonce(i, last), MAGIC, data.into(), &tag)
            .map_err(|_| bad("vault file is corrupt or was tampered with"))?;
        w.write_all(data)?;
        if last {
            return w.flush();
        }
        i += 1;
    }
}

/// Writes `w` to a temp file next to `path`, then moves it into place (readers never see half a
/// file). `durable` flushes it to the disk first (~ms per file): skip that for regenerable caches.
fn replace(path: &Path, durable: bool, w: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>) -> io::Result<()> {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{:016x}.tmp", u64::from_le_bytes(r)));
    let tmp: PathBuf = path.with_file_name(name);
    let res = (|| {
        let mut f = BufWriter::new(File::create(&tmp)?);
        w(&mut f)?;
        let f = f.into_inner().map_err(|e| e.into_error())?;
        if durable {
            f.sync_all()?;
        }
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Encrypts `data` into `path`, flushed to the disk.
pub fn write(path: &Path, data: &[u8]) -> io::Result<()> {
    replace(path, true, |f| encrypt(data, f))
}

/// `write` for files that can be regenerated (thumbnails): no flush to the disk.
pub fn write_cache(path: &Path, data: &[u8]) -> io::Result<()> {
    replace(path, false, |f| encrypt(data, f))
}

/// Decrypts the whole file into memory.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    let f = File::open(path)?;
    let mut out = Vec::with_capacity(f.metadata().map(|m| m.len() as usize).unwrap_or(0));
    decrypt(f, &mut out)?;
    Ok(out)
}

/// Decrypts `src` into the plain file `dst`, streaming.
pub fn decrypt_file(src: &Path, dst: &Path) -> io::Result<()> {
    let r = File::open(src)?;
    replace(dst, false, |f| decrypt(r, f))
}

/// Encrypts the plain file `src` into `dst` (may be the same path), streaming.
pub fn encrypt_file(src: &Path, dst: &Path, durable: bool) -> io::Result<()> {
    let r = File::open(src)?;
    replace(dst, durable, |f| encrypt(r, f))
}

/// Whether `path` is already in vault format.
pub fn is_sealed(path: &Path) -> bool {
    let mut m = [0u8; 4];
    File::open(path).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(data: &[u8]) -> Vec<u8> {
        init([7; 32]);
        let mut out = Vec::new();
        encrypt(data, &mut out).unwrap();
        out
    }

    fn open(sealed: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        decrypt(sealed, &mut out).map(|_| out)
    }

    #[test]
    fn roundtrip_at_chunk_edges() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31) as u8).collect();
            let sealed = seal(&data);
            assert_eq!(sealed.len(), 4 + SALT + len + (len / CHUNK + 1) * TAG, "len {len}");
            assert_eq!(open(&sealed).unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn same_input_seals_differently() {
        assert_ne!(seal(b"hello"), seal(b"hello"));
    }

    #[test]
    fn rejects_tampering_and_truncation() {
        let data = vec![5u8; 2 * CHUNK + 10];
        let sealed = seal(&data);
        let mut flipped = sealed.clone();
        flipped[100] ^= 1;
        assert!(open(&flipped).is_err());
        // cut after the first full chunk: that chunk was sealed as "not last"
        assert!(open(&sealed[..4 + SALT + CHUNK + TAG]).is_err());
        assert!(open(&sealed[..sealed.len() - 1]).is_err());
        assert!(open(b"plain png bytes, not sealed").is_err());
    }
}
