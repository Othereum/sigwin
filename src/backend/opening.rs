//! A downloaded attachment opened as it comes in, so it never has to be whole in memory: the
//! ciphertext arrives in pieces, the plain bytes come out as a `Read` for the vault to seal.
//!
//! What comes out before the end isn't authenticated yet: the MAC and the digest are checked
//! once the last piece is in, and a mismatch fails the read. Whoever writes it out must drop the
//! file then (`sigwin_vault::write_from` does).

use std::io::{self, Read};

use aes::cipher::consts::{U16, U32, U64};
use aes::cipher::{Array, BlockModeDecrypt, KeyIvInit};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// IV, at least one block, MAC.
const IV: usize = 16;
const MAC: usize = 32;
const BLOCK: usize = 16;

pub(super) struct Opening<I> {
    pieces: I,
    key: [u8; 64],
    digest: Option<Vec<u8>>,
    /// plain bytes still to give out; the rest is padding. None: all of them.
    left: Option<u64>,
    aes: Option<cbc::Decryptor<aes::Aes256>>,
    mac: Hmac<Sha256>,
    sha: Sha256,
    /// ciphertext not decrypted yet: the last block and the MAC are held back until the end
    held: Vec<u8>,
    out: Vec<u8>,
    out_at: usize,
    done: bool,
}

impl<I: Iterator<Item = io::Result<Vec<u8>>>> Opening<I> {
    /// `pieces` ends (None) when the download is complete; an error in it fails the read.
    /// `size`: the plain size the message gives.
    pub(super) fn new(pieces: I, key: [u8; 64], digest: Option<Vec<u8>>, size: Option<u64>) -> Self {
        Opening {
            pieces,
            key,
            digest,
            left: size,
            aes: None,
            mac: Hmac::<Sha256>::new_from_slice(&key[32..]).expect("fixed length key"),
            sha: Sha256::new(),
            held: Vec::new(),
            out: Vec::new(),
            out_at: 0,
            done: false,
        }
    }

    fn feed(&mut self, piece: &[u8]) {
        self.sha.update(piece);
        self.held.extend_from_slice(piece);
        if self.aes.is_none() {
            if self.held.len() < IV {
                return;
            }
            let iv: [u8; IV] = self.held[..IV].try_into().expect("IV length");
            self.mac.update(&iv);
            let key: &Array<u8, U64> = (&self.key).into();
            self.aes = Some(cbc::Decryptor::new(key.split_ref::<U32>().0, &iv.into()));
            self.held.drain(..IV);
        }
        if self.held.len() > MAC + BLOCK {
            let n = (self.held.len() - MAC - BLOCK) / BLOCK * BLOCK;
            self.open(n);
            self.held.drain(..n);
        }
    }

    /// Decrypts the first `n` held bytes and gives out what isn't padding.
    fn open(&mut self, n: usize) {
        self.mac.update(&self.held[..n]);
        let (blocks, _) = Array::<u8, U16>::slice_as_chunks_mut(&mut self.held[..n]);
        self.aes.as_mut().expect("IV read").decrypt_blocks(blocks);
        self.give(n);
    }

    fn give(&mut self, n: usize) {
        let take = self.left.map_or(n, |l| (l as usize).min(n));
        self.out.extend_from_slice(&self.held[..take]);
        if let Some(l) = &mut self.left {
            *l -= take as u64;
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.aes.is_none() || self.held.len() < MAC + BLOCK || (self.held.len() - MAC) % BLOCK != 0 {
            return Err(bad("the attachment is cut short"));
        }
        let end = self.held.len() - MAC;
        self.mac.update(&self.held[..end]);
        self.mac.clone().verify_slice(&self.held[end..]).map_err(|_| bad("attachment MAC doesn't match"))?;
        if let Some(want) = &self.digest {
            if self.sha.clone().finalize()[..] != want[..] {
                return Err(bad("attachment digest doesn't match"));
            }
        }
        let (blocks, _) = Array::<u8, U16>::slice_as_chunks_mut(&mut self.held[..end]);
        self.aes.as_mut().expect("IV read").decrypt_blocks(blocks);
        let pad = self.held[end - 1] as usize;
        if pad == 0 || pad > BLOCK || self.held[end - pad..end].iter().any(|b| *b as usize != pad) {
            return Err(bad("attachment padding is wrong"));
        }
        self.give(end - pad);
        Ok(())
    }
}

impl<I: Iterator<Item = io::Result<Vec<u8>>>> Read for Opening<I> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.out_at < self.out.len() {
                let n = buf.len().min(self.out.len() - self.out_at);
                buf[..n].copy_from_slice(&self.out[self.out_at..self.out_at + n]);
                self.out_at += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            self.out.clear();
            self.out_at = 0;
            match self.pieces.next() {
                Some(Ok(piece)) => self.feed(&piece),
                Some(Err(e)) => return Err(e),
                None => {
                    self.done = true;
                    self.finish()?;
                }
            }
        }
    }
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// The pieces a download hands over from the async side: `None` marks the end, and a channel
/// closed before it means the download stopped.
pub(super) struct Pieces(pub(super) tokio::sync::mpsc::Receiver<Option<Vec<u8>>>);

impl Iterator for Pieces {
    type Item = io::Result<Vec<u8>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.0.blocking_recv() {
            Some(Some(piece)) => Some(Ok(piece)),
            Some(None) => None,
            // not `Interrupted`: readers take that as "try again"
            None => Some(Err(io::Error::other("the download stopped"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::attachment_cipher::encrypt_in_place;

    fn sealed(plain: &[u8], padded: usize, key: [u8; 64]) -> Vec<u8> {
        let mut data = plain.to_vec();
        data.resize(padded.max(plain.len()), 0);
        encrypt_in_place([9; 16], key, &mut data);
        data
    }

    /// Opens `cipher` handed over in pieces of `step` bytes.
    fn open(cipher: &[u8], step: usize, key: [u8; 64], digest: Option<Vec<u8>>, size: Option<u64>) -> io::Result<Vec<u8>> {
        let pieces = cipher.chunks(step).map(|c| Ok(c.to_vec())).collect::<Vec<_>>().into_iter();
        let mut out = Vec::new();
        Opening::new(pieces, key, digest, size).read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn pieces_of_any_size_open_to_the_plain_bytes() {
        let key = [5; 64];
        for n in [0, 1, 15, 16, 17, 100, 4096, 70_001] {
            let plain: Vec<u8> = (0..n).map(|i| (i * 31 % 253) as u8).collect();
            let cipher = sealed(&plain, n + n / 20 + 3, key);
            let digest = Sha256::digest(&cipher).to_vec();
            for step in [1, 7, 16, 48, 49, 1000, cipher.len()] {
                assert_eq!(open(&cipher, step, key, Some(digest.clone()), Some(n as u64)).unwrap(), plain, "{n} bytes in {step}s");
            }
        }
    }

    #[test]
    fn downloads_are_checked_and_unpadded() {
        let key = [5; 64];
        let cipher = sealed(b"hello video", 16, key);
        let digest = Sha256::digest(&cipher).to_vec();
        assert_eq!(open(&cipher, 5, key, Some(digest.clone()), Some(11)).unwrap(), b"hello video");
        // without a size the zeros stay; a size past the end doesn't pad
        assert_eq!(open(&cipher, 5, key, Some(digest.clone()), None).unwrap().len(), 16);
        assert_eq!(open(&cipher, 5, key, Some(digest.clone()), Some(99)).unwrap().len(), 16);
        // history from the phone has no digest: the MAC still checks it
        assert_eq!(open(&cipher, 5, key, None, Some(11)).unwrap(), b"hello video");
        assert!(open(&cipher, 5, key, Some(vec![0; 32]), None).is_err());
        assert!(open(&cipher, 5, [6; 64], None, None).is_err());
        let mut flipped = cipher.clone();
        flipped[20] ^= 1;
        assert!(open(&flipped, 5, key, None, None).is_err());
        assert!(open(&cipher[..cipher.len() - 1], 5, key, None, None).is_err());
        assert!(open(&cipher[..20], 5, key, None, None).is_err());
    }

    #[test]
    fn a_download_that_stops_fails_the_read() {
        let key = [5; 64];
        let cipher = sealed(&[1; 100], 100, key);
        let pieces = vec![Ok(cipher[..50].to_vec()), Err(io::Error::other("stopped"))].into_iter();
        let mut out = Vec::new();
        assert!(Opening::new(pieces, key, None, None).read_to_end(&mut out).is_err());
    }
}
