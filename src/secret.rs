//! Random database passphrase, kept on disk encrypted with Windows DPAPI (bound to the
//! current Windows user), so the sqlcipher database is useless if copied elsewhere.

use std::path::Path;

use anyhow::{bail, Context};

pub fn load_or_create(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        let blob = std::fs::read(path).context("reading key file")?;
        let plain = unprotect(&blob).context("decrypting key file (DPAPI)")?;
        return String::from_utf8(plain).context("key file is not utf-8");
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let pass = hex::encode(raw);
    let blob = protect(pass.as_bytes())?;
    std::fs::write(path, blob).context("writing key file")?;
    Ok(pass)
}

#[cfg(windows)]
fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    dpapi(data, true)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    dpapi(data, false)
}

#[cfg(windows)]
fn dpapi(data: &[u8], encrypt: bool) -> anyhow::Result<Vec<u8>> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    // SAFETY: input points to `data` for the duration of the call; the output buffer is
    // allocated by DPAPI, copied out, and released with LocalFree.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            bail!("DPAPI call failed: {}", std::io::Error::last_os_error());
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Ok(v)
    }
}

#[cfg(not(windows))]
fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(data.to_vec())
}
