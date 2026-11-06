//! Self-update from GitHub Releases: check the latest release, download the installer over
//! HTTPS, verify it against the release's SHA256SUMS, then run it silently and relaunch.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REPO: &str = "Saroknight/stream.eventworks";
const SETUP: &str = "Streamroom-Live-Setup.exe";
const SUMS: &str = "SHA256SUMS";

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

/// "0.10.2" > "0.9.9", ignoring a leading "app-v" / "v".
fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches("app-v").trim_start_matches('v').split(['.', '-']).map_while(|p| p.parse().ok()).collect()
    };
    parse(candidate) > parse(current)
}

pub async fn check() -> Result<Option<Available>> {
    let r: Release = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let version = r.tag_name.trim_start_matches("app-v").trim_start_matches('v').to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let find = |n: &str| r.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let (Some(setup_url), Some(sums_url)) = (find(SETUP), find(SUMS)) else { return Ok(None) };
    Ok(Some(Available { version, notes: r.body.chars().take(600).collect(), setup_url, sums_url }))
}

/// Downloads and verifies the installer, starts it (after this app exits), and returns.
/// The caller should exit right after.
pub async fn install(a: &Available) -> Result<()> {
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
    let setup = std::env::temp_dir().join(format!("Streamroom-Live-Setup-{}.exe", a.version));
    std::fs::write(&setup, &bytes).context("saving the installer")?;
    run_after_exit(&setup)
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

#[cfg(not(windows))]
fn run_after_exit(_setup: &std::path::Path) -> Result<()> {
    bail!("updates are only installed on Windows")
}

#[cfg(test)]
mod tests {
    #[test]
    fn compares_versions() {
        assert!(super::newer("app-v0.2.1", "0.2.0"));
        assert!(super::newer("0.10.0", "0.9.9"));
        assert!(!super::newer("app-v0.2.0", "0.2.0"));
        assert!(!super::newer("0.1.9", "0.2.0"));
    }
}
