//! Where this PC's sign-in lives: one small file in the app's config folder. On Windows it's
//! encrypted with DPAPI, so only this Windows user on this PC can read it. On Linux it's a plain
//! file only this user can read (mode 0600).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use streamroom_core::{log, Account};

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("account.dat")
}

pub fn load(config_dir: &Path) -> Option<Account> {
    let p = path(config_dir);
    let raw = std::fs::read(&p).ok()?;
    // Older versions wrote it readable by everyone (the default umask): tighten it.
    if let Err(e) = make_private(&p) {
        log::info(&format!("couldn't restrict who can read {}: {e}", p.display()));
    }
    let plain = unprotect(&raw).ok()?;
    serde_json::from_slice(&plain).ok()
}

pub fn save(config_dir: &Path, a: &Account) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let sealed = protect(&serde_json::to_vec(a)?).context("encrypting the sign-in")?;
    let p = path(config_dir);
    let tmp = p.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    // Restricted before the key is written, so it's never readable by others.
    make_private(&tmp).with_context(|| format!("restricting who can read {}", tmp.display()))?;
    file.write_all(&sealed).with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, &p).with_context(|| format!("replacing {}", p.display()))?;
    Ok(())
}

pub fn forget(config_dir: &Path) {
    let p = path(config_dir);
    if let Err(e) = std::fs::remove_file(&p) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::info(&format!("couldn't delete the sign-in {}: {e}", p.display()));
        }
    }
}

/// Only this user may read or write the file. (Windows: DPAPI already ties it to this user.)
#[cfg(unix)]
fn make_private(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private(_p: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A short name for this PC, shown on the admin page ("Windows app · GAMING-PC").
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "PC".into())
}

#[cfg(windows)]
fn dpapi(data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
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

// Other systems: a plain file, readable only by this user (see `make_private`).
#[cfg(not(windows))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}
#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_the_sign_in() {
        let dir = std::env::temp_dir().join(format!("streamroom-account-test-{}", std::process::id()));
        let a = Account { server: "https://s".into(), user_id: "u".into(), user_name: "n".into(), is_admin: false, device_key: "k".into() };
        save(&dir, &a).unwrap();
        let back = load(&dir).unwrap();
        assert_eq!((back.server, back.device_key), (a.server, a.device_key));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "only this user may read the key");
        }
        forget(&dir);
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
