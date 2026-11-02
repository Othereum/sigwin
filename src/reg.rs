//! Minimal HKCU registry access (no admin rights needed for anything we write).

#![cfg(windows)]

use anyhow::bail;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
    RRF_RT_REG_SZ,
};

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn check(rc: u32, what: &str, name: &str) -> anyhow::Result<()> {
    if rc != ERROR_SUCCESS {
        bail!("{what} {name}: {rc}");
    }
    Ok(())
}

pub fn set_str(key: &str, name: &str, value: &str) -> anyhow::Result<()> {
    let (k, n, v) = (w(key), w(name), w(value));
    // SAFETY: all pointers are NUL-terminated UTF-16 buffers alive for the call
    let rc = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32)
    };
    check(rc, "RegSetKeyValueW", name)
}

pub fn set_dword(key: &str, name: &str, value: u32) -> anyhow::Result<()> {
    let (k, n) = (w(key), w(name));
    // SAFETY: NUL-terminated strings, a 4-byte value
    let rc = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_DWORD, (&value as *const u32).cast(), 4)
    };
    check(rc, "RegSetKeyValueW", name)
}

/// `None` when the key or value is missing (or not a string).
pub fn get_str(key: &str, name: &str) -> Option<String> {
    let (k, n) = (w(key), w(name));
    let mut buf = vec![0u16; 512];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: buffer and its byte length match; strings are NUL-terminated
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(bytes as usize / 2);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf16_lossy(&buf))
}

/// Deletes a key and everything under it. Missing is fine.
pub fn delete_tree(key: &str) -> anyhow::Result<()> {
    let k = w(key);
    // SAFETY: NUL-terminated string
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, k.as_ptr()) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(rc, "RegDeleteTreeW", key)?;
    // RegDeleteTreeW empties the key but leaves it; remove the key itself
    let rc = unsafe { windows_sys::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, k.as_ptr()) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(rc, "RegDeleteKeyW", key)
}

/// Missing is fine.
pub fn delete_value(key: &str, name: &str) -> anyhow::Result<()> {
    let (k, n) = (w(key), w(name));
    // SAFETY: NUL-terminated strings
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr()) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(rc, "RegDeleteKeyValueW", name)
}
