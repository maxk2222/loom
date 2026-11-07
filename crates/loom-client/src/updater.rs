//! Client updater: release manifest check, signed tarball download + verify,
//! staged swap with previous-binary rollback. Restart via systemd (or manual).

use anyhow::{bail, Context, Result};
use loom_core::config::UpdaterConfig;
use serde::Deserialize;
use sha2::Digest;
use std::path::PathBuf;
use tracing::{info, warn};

/// Built-in LOOM release signing key (minisign public key, base64).
/// Overridable per client via `updater.public_key` or `LOOM_UPDATE_PUBKEY`.
pub const DEFAULT_UPDATE_PUBKEY: &str = "RWTQJasAh8M2eVLthz0gOspXSnE7cYG1a8UovkXa+T3Sm/0gAZGU4WCS";

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
/// Returns the status plus the parsed manifest (when a release exists).
pub async fn check_for_update(
    server_http_base: &str,
    channel: &str,
    arch: &str,
    current: &str,
) -> Result<(UpdateStatus, Option<ReleaseManifest>)> {
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
        return Ok((
            UpdateStatus {
                enabled: true,
                channel: channel.into(),
                current_version: current.into(),
                latest_version: None,
                update_available: false,
                last_error: None,
            },
            None,
        ));
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
    let st = UpdateStatus {
        enabled: true,
        channel: channel.into(),
        current_version: current.into(),
        latest_version: Some(mf.version.clone()),
        update_available,
        last_error: None,
    };
    Ok((st, Some(mf)))
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
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        (os, arch) => {
            warn!(os, arch, "unknown update arch");
            "unknown"
        }
    }
}

/// HTTP base URL serving `/api/releases/*` on the server.
/// Prefers `updater.server_base`; otherwise derives it from the gRPC
/// `server_url` by swapping the default agent port for the HTTP port.
pub fn manifest_base(cfg: &UpdaterConfig, server_url: &str) -> String {
    if !cfg.server_base.is_empty() {
        return cfg.server_base.trim_end_matches('/').into();
    }
    let u = server_url.trim_end_matches('/');
    if u.is_empty() {
        return String::new();
    }
    // gRPC agent :50051 → HTTP dashboard :8080 (LOOM default ports).
    if let Some(host) = u.strip_suffix(":50051") {
        return format!("{host}:8080");
    }
    u.into()
}

fn public_key(cfg: &UpdaterConfig) -> String {
    if !cfg.public_key.is_empty() {
        return cfg.public_key.clone();
    }
    std::env::var("LOOM_UPDATE_PUBKEY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_UPDATE_PUBKEY.into())
}

/// One update cycle: check the manifest; when `force` or `auto_apply` and an
/// update is available → download, verify, stage, swap, then exit so the
/// supervisor (systemd/launchd) restarts us on the new binary.
/// Returns the status *before* any apply; on success this call never returns.
pub async fn run_once(
    cfg: &UpdaterConfig,
    server_url: &str,
    current: &str,
    stage_dir: &std::path::Path,
    force: bool,
) -> Result<UpdateStatus> {
    let base = manifest_base(cfg, server_url);
    if base.is_empty() {
        bail!("no manifest server base configured");
    }
    let (st, mf) = check_for_update(&base, &cfg.channel, arch(), current).await?;
    let Some(mf) = mf else { return Ok(st) };
    if !st.update_available || !(force || cfg.auto_apply) {
        return Ok(st);
    }
    let staged = download_and_stage(&mf, &public_key(cfg), stage_dir).await?;
    info!(version = %mf.version, "update staged; swapping binary and restarting");
    apply_staged(&staged)?; // keeps a .prev backup for manual rollback
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in release key must verify a real minisign CLI signature
    /// (multi-line `.minisig` content, prehashed mode) — mirrors what the
    /// release workflow emits.
    #[test]
    fn builtin_key_verifies_cli_signature() {
        let payload = b"hello\n";
        let sig_text = "untrusted comment: signature from minisign secret key\n\
RUTQJasAh8M2efBrivYMVX75WosU9RvAqlhT8MdxVmM8c3bscKzudQoXQQBAVfX85tinhyreFAYFBheEFOguY5cIJuvfOKjocA0=\n\
trusted comment: timestamp:1794079809\tfile:t.bin\thashed\n\
imqqDIPsV8ArLIM/Z0azYXQxuB1Hsrejc2NLm9d6DF1Uinc7ZBO9YC0AtUZV1kov9p1u7QEJmtgqHGEWZ3wLDg==\n";
        let pk = minisign_verify::PublicKey::from_base64(DEFAULT_UPDATE_PUBKEY).unwrap();
        let sig = minisign_verify::Signature::decode(sig_text).unwrap();
        pk.verify(payload, &sig, false).unwrap();
        assert!(pk.verify(b"wrong", &sig, false).is_err());
    }

    #[test]
    fn manifest_base_derivation() {
        let mut cfg = UpdaterConfig::default();
        assert_eq!(manifest_base(&cfg, "http://host:50051"), "http://host:8080");
        assert_eq!(manifest_base(&cfg, "grpc://a:9999"), "grpc://a:9999");
        cfg.server_base = "https://l.example.com/".into();
        assert_eq!(
            manifest_base(&cfg, "http://host:50051"),
            "https://l.example.com"
        );
    }
}
