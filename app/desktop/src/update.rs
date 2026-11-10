//! Self-update from GitHub Releases: check the latest release, download the installer over
//! HTTPS, verify it against the release's SHA256SUMS, then install it and relaunch.
//! Windows: the NSIS installer, run silently after this app exits.
//! Linux (Arch): the pacman package, installed with `pkexec pacman -U` (the desktop asks for
//! the password), then the new version is started.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

pub const REPO: &str = "Saroknight/stream.eventworks";
#[cfg(not(target_os = "linux"))]
const SETUP: &str = "Streamroom-Live-Setup.exe";
#[cfg(target_os = "linux")]
const SETUP: &str = "streamroom-live-x86_64.pkg.tar.zst";
const SUMS: &str = "SHA256SUMS";
/// Each platform has its own releases: Windows is app-v<version>, Linux is linux-v<version>.
#[cfg(not(target_os = "linux"))]
const TAG: &str = "app-v";
#[cfg(target_os = "linux")]
const TAG: &str = "linux-v";

#[derive(Clone, Debug, Serialize)]
pub struct Available {
    pub version: String,
    pub notes: String,
    #[serde(skip)]
    setup_url: String,
    #[serde(skip)]
    sums_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("streamroom-live/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()?)
}

/// "0.10.2" > "0.9.9", ignoring a leading tag prefix ("app-v", "linux-v", "v").
fn newer(candidate: &str, current: &str) -> bool {
    let parse =
        |v: &str| -> Vec<u64> { v.trim_start_matches(|c: char| !c.is_ascii_digit()).split(['.', '-']).map_while(|p| p.parse().ok()).collect() };
    parse(candidate) > parse(current)
}

pub async fn check() -> Result<Option<Available>> {
    let list: Vec<Release> = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // Newest first; the first published release of this platform is the one to compare with.
    let Some(r) = list.into_iter().find(|r| !r.draft && !r.prerelease && r.tag_name.starts_with(TAG)) else {
        return Ok(None);
    };
    let version = r.tag_name[TAG.len()..].to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let find = |n: &str| r.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let (Some(setup_url), Some(sums_url)) = (find(SETUP), find(SUMS)) else { return Ok(None) };
    Ok(Some(Available { version, notes: r.body.chars().take(600).collect(), setup_url, sums_url }))
}

/// Downloads and verifies the installer, starts it (after this app exits), and returns.
/// The caller should exit right after. `cache`: the app's cache folder.
pub async fn install(a: &Available, cache: &Path) -> Result<()> {
    let c = client()?;
    let sums = c.get(&a.sums_url).send().await?.error_for_status()?.text().await?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(SETUP))
        .and_then(|l| l.split_whitespace().next())
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| anyhow!("the release has no checksum for the installer"))?;
    let bytes = c.get(&a.setup_url).send().await?.error_for_status()?.bytes().await?;
    let actual = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
    if actual != expected {
        bail!("the downloaded installer didn't match its checksum, so it wasn't run");
    }
    let (cache, name) = (cache.to_path_buf(), format!("{}-{}", a.version, SETUP));
    // Saving and installing block (on Linux until the password is typed): keep them off the
    // async runtime.
    tokio::task::spawn_blocking(move || {
        let setup = download_dir(&cache).context("making a folder for the installer")?.join(name);
        std::fs::write(&setup, &bytes).context("saving the installer")?;
        run_after_exit(&setup)
    })
    .await
    .context("installing the update")?
}

/// Where the installer is saved: `update/` in the app's cache folder, made fresh each time and
/// open to this user only. It's installed as root, so it mustn't sit under a name other users
/// could take or swap (as in the shared temp folder).
#[cfg(unix)]
fn download_dir(cache: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = cache.join("update");
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("emptying {}", dir.display())),
        Ok(()) | Err(_) => {}
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // The mode above is cut down by the umask but never widened; this sets it exactly.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).with_context(|| format!("restricting {}", dir.display()))?;
    Ok(dir)
}

/// Windows: the user's own temp folder, as before.
#[cfg(not(unix))]
fn download_dir(_cache: &Path) -> Result<PathBuf> {
    Ok(std::env::temp_dir())
}

/// Install the package (pacman asks for the password through the desktop), then start the new
/// version; the caller exits right after.
#[cfg(target_os = "linux")]
fn run_after_exit(setup: &std::path::Path) -> Result<()> {
    let status =
        std::process::Command::new("pkexec").args(["pacman", "-U", "--noconfirm"]).arg(setup).status().context("starting pkexec (polkit)")?;
    if !status.success() {
        bail!("the update wasn't installed (password cancelled, or pacman refused it)");
    }
    if let Err(e) = std::fs::remove_file(setup) {
        warn!("couldn't delete the downloaded update {}: {e}", setup.display());
    }
    // Start the new version once this one has exited.
    let pid = std::process::id();
    std::process::Command::new("sh")
        .args(["-c", &format!("while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; exec streamroom-live")])
        .spawn()
        .context("relaunching")?;
    Ok(())
}

#[cfg(windows)]
fn run_after_exit(setup: &std::path::Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe()?;
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    // Wait for us to exit, install silently (per-user, no admin prompt), then start the new version.
    let script = format!(
        "try {{ Wait-Process -Id {} -Timeout 30 -ErrorAction SilentlyContinue }} catch {{}}; \
         Start-Process -FilePath '{}' -ArgumentList '/S' -Wait; Start-Process -FilePath '{}'",
        std::process::id(),
        q(setup),
        q(&exe)
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("starting the installer")?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run_after_exit(_setup: &std::path::Path) -> Result<()> {
    bail!("updates aren't installed automatically on this system")
}

#[cfg(test)]
mod tests {
    /// The package is installed as root: its folder is ours alone and holds nothing left over.
    #[cfg(unix)]
    #[test]
    fn the_download_folder_is_private_and_empty() {
        use std::os::unix::fs::PermissionsExt;
        let cache = std::env::temp_dir().join(format!("streamroom-update-test-{}", std::process::id()));
        let dir = cache.join("update");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::write(dir.join("left-over.pkg.tar.zst"), b"old").unwrap();

        assert_eq!(super::download_dir(&cache).unwrap(), dir);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // And when there was no folder yet.
        std::fs::remove_dir_all(&cache).unwrap();
        assert_eq!(super::download_dir(&cache).unwrap(), dir);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(&cache).unwrap();
    }

    #[test]
    fn compares_versions() {
        assert!(super::newer("app-v0.2.1", "0.2.0"));
        assert!(super::newer("0.10.0", "0.9.9"));
        assert!(!super::newer("app-v0.2.0", "0.2.0"));
        assert!(!super::newer("0.1.9", "0.2.0"));
    }
}
