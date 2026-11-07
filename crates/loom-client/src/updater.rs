//! Client updater: release manifest check, signed tarball download + verify,
//! staged swap with previous-binary rollback. Restart via systemd (or manual).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::Digest;
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    pub channel: String,
    pub url: String,
    pub sha256: String,
    pub signature: String, // minisign signature (base64 or detached content)
    pub min_version_protocol: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateStatus {
    pub enabled: bool,
    pub channel: String,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub last_error: Option<String>,
}

/// Check the server's release manifest endpoint for an update.
pub async fn check_for_update(
    server_http_base: &str,
    channel: &str,
    arch: &str,
    current: &str,
) -> Result<UpdateStatus> {
    let url = format!(
        "{}/api/releases/manifest?channel={}&arch={}",
        server_http_base.trim_end_matches('/'),
        channel,
        arch
    );
    let http = reqwest::Client::new();
    let resp = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .context("manifest request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateStatus {
            enabled: true,
            channel: channel.into(),
            current_version: current.into(),
            latest_version: None,
            update_available: false,
            last_error: None,
        });
    }
    let mf: ReleaseManifest = resp
        .error_for_status()
        .context("manifest status")?
        .json()
        .await
        .context("manifest parse")?;
    let update_available = semver::Version::parse(&mf.version)
        .map(|v| {
            v > semver::Version::parse(current).unwrap_or_else(|_| semver::Version::new(0, 0, 0))
        })
        .unwrap_or(false);
    Ok(UpdateStatus {
        enabled: true,
        channel: channel.into(),
        current_version: current.into(),
        latest_version: Some(mf.version),
        update_available,
        last_error: None,
    })
}

/// Download + verify + stage an update. Returns the staged binary path.
/// `public_key`: minisign public key embedded in the binary/config.
pub async fn download_and_stage(
    manifest_url_manifest: &ReleaseManifest,
    public_key: &str,
    dest_dir: &std::path::Path,
) -> Result<PathBuf> {
    let http = reqwest::Client::new();
    let bytes = http
        .get(&manifest_url_manifest.url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .context("download failed")?
        .error_for_status()
        .context("download status")?
        .bytes()
        .await
        .context("read body")?;

    // sha256 check (the artifact is the raw binary, signed with minisign)
    let computed = hex::encode(sha2::Sha256::digest(&bytes));
    if !computed.eq_ignore_ascii_case(&manifest_url_manifest.sha256) {
        bail!(
            "sha256 mismatch: expected {}, got {}",
            manifest_url_manifest.sha256,
            computed
        );
    }

    // minisign verify (detached signature over the binary bytes)
    let pubkey = minisign_verify::PublicKey::from_base64(public_key)
        .map_err(|e| anyhow::anyhow!("bad public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(&manifest_url_manifest.signature)
        .map_err(|e| anyhow::anyhow!("bad signature: {e}"))?;
    pubkey
        .verify(&bytes, &sig, false)
        .map_err(|e| anyhow::anyhow!("signature verify failed: {e}"))?;

    std::fs::create_dir_all(dest_dir).ok();
    let staged = dest_dir.join("loom-client.new");
    std::fs::write(&staged, &bytes).context("write staged binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&staged)?.permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&staged, p)?;
    }
    Ok(staged)
}

/// Atomically swap the current executable with a staged binary; keeps a .prev
/// backup for rollback. After this returns, the process should exit and let
/// systemd restart it.
pub fn apply_staged(staged: &std::path::Path) -> Result<PathBuf> {
    let current = std::env::current_exe().context("current_exe")?;
    let prev = current.with_extension("prev");
    std::fs::rename(&current, &prev).context("backup current binary")?;
    if let Err(e) = std::fs::rename(staged, &current) {
        // rollback best-effort
        let _ = std::fs::rename(&prev, &current);
        return Err(e).context("install staged binary");
    }
    Ok(prev)
}

/// Roll back to the .prev binary (if present) and exit for restart.
pub fn rollback() -> Result<()> {
    let current = std::env::current_exe()?;
    let prev = current.with_extension("prev");
    if !prev.exists() {
        bail!("no previous binary to roll back to");
    }
    std::fs::rename(&prev, &current)?;
    Ok(())
}

pub fn arch() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        (os, arch) => {
            warn!(os, arch, "unknown update arch");
            "unknown"
        }
    }
}
