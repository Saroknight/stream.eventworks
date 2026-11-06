//! Where this PC's sign-in lives: one small file in the app's config folder. On Windows it's
//! encrypted with DPAPI, so only this Windows user on this PC can read it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use streamroom_core::Account;

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("account.dat")
}

pub fn load(config_dir: &Path) -> Option<Account> {
    let raw = std::fs::read(path(config_dir)).ok()?;
    let plain = unprotect(&raw).ok()?;
    serde_json::from_slice(&plain).ok()
}

pub fn save(config_dir: &Path, a: &Account) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let sealed = protect(&serde_json::to_vec(a)?).context("encrypting the sign-in")?;
    let p = path(config_dir);
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, sealed)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

pub fn forget(config_dir: &Path) {
    let _ = std::fs::remove_file(path(config_dir));
}

/// A short name for this PC, shown on the admin page ("Windows app · GAMING-PC").
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "PC".into())
}

#[cfg(windows)]
fn dpapi(data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    unsafe {
        if encrypt {
            CryptProtectData(&input, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        Ok(v)
    }
}

#[cfg(windows)]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, true)
}
#[cfg(windows)]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, false)
}

// Development builds on other systems: plain file (the app only ships for Windows).
#[cfg(not(windows))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}
#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}
