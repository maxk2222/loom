//! Client updater: release manifest check, signed tarball download + verify,
//! staged swap with previous-binary rollback. Restart via systemd (or manual).

use anyhow::{bail, Context, Result};
use loom_core::config::UpdaterConfig;
use serde::Deserialize;
use sha2::Digest;
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

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
    apply_staged_to(staged, &current)
}

/// Swap `current` with `staged`, keeping a `.prev` backup next to it.
/// Split from `apply_staged` so tests can drive the swap without being the
/// running executable.
pub fn apply_staged_to(staged: &std::path::Path, current: &std::path::Path) -> Result<PathBuf> {
    let prev = current.with_extension("prev");
    std::fs::rename(current, &prev).context("backup current binary")?;
    if let Err(e) = std::fs::rename(staged, current) {
        // rollback best-effort
        let _ = std::fs::rename(&prev, current);
        return Err(e).context("install staged binary");
    }
    Ok(prev)
}

/// Roll back to the .prev binary (if present) and exit for restart.
pub fn rollback() -> Result<()> {
    let current = std::env::current_exe()?;
    rollback_at(&current)
}

/// Restore `<current>.prev` over `current`. Split for testability.
pub fn rollback_at(current: &std::path::Path) -> Result<()> {
    let prev = current.with_extension("prev");
    if !prev.exists() {
        bail!("no previous binary to roll back to");
    }
    std::fs::rename(&prev, current)?;
    Ok(())
}

// ---------- boot-failure watchdog (auto-rollback) ----------
//
// After a binary swap the updater writes `.loom-update-pending` next to the
// executable. Each process start while the marker exists counts one boot;
// a boot is confirmed either by a successful server registration or by
// `HEALTHY_SECS` of uptime (the binary works even when the server is down).
// If the marker is still there after `MAX_FAILED_BOOTS` starts — the new
// binary kept crashing — the watchdog restores `.prev` automatically and
// exits so the supervisor restarts the old binary, ending the crash loop.

/// Boots tolerated before the new binary is declared bricked.
const MAX_FAILED_BOOTS: u32 = 3;
/// Uptime that alone confirms a healthy boot (covers offline operation).
pub const HEALTHY_SECS: u64 = 120;
const PENDING_MARKER: &str = ".loom-update-pending";
const ROLLBACK_RECORD: &str = ".loom-rollback.json";

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
struct PendingBoot {
    boots: u32,
    version: String,
    applied_at: i64,
}

fn binary_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run at process start (before config load): bump the boot counter and, on
/// exceeding the threshold, roll back and exit — split into a pure helper so
/// tests can drive it without touching the real executable.
pub fn boot_watchdog() {
    let (Some(dir), Ok(current)) = (binary_dir(), std::env::current_exe()) else {
        return;
    };
    match boot_check(&dir, &current) {
        BootVerdict::RollBack => {
            std::process::exit(0); // supervisor restarts the restored binary
        }
        BootVerdict::Pending { boots } => {
            info!(boots, "update pending — boot counted");
        }
        BootVerdict::Clean => {}
    }
}

enum BootVerdict {
    Clean,
    Pending { boots: u32 },
    RollBack,
}

/// Read/increment the pending marker in `dir`; on threshold restore `.prev`
/// over `current` right there (rename is synchronous and safe mid-boot).
fn boot_check(dir: &std::path::Path, current: &std::path::Path) -> BootVerdict {
    let marker = dir.join(PENDING_MARKER);
    if !marker.exists() {
        return BootVerdict::Clean;
    }
    let mut st: PendingBoot = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    st.boots += 1;
    if st.boots <= MAX_FAILED_BOOTS {
        if let Err(e) = std::fs::write(&marker, serde_json::to_string(&st).unwrap_or_default()) {
            warn!(error = %e, "could not update boot marker");
        }
        return BootVerdict::Pending { boots: st.boots };
    }
    // Crash loop: restore .prev and record why.
    let outcome = match rollback_at(current) {
        Ok(()) => "restored .prev binary",
        Err(e) => {
            error!(error = %e, "auto-rollback failed — no .prev or rename error");
            "rollback failed"
        }
    };
    let record = serde_json::json!({
        "rolled_back_at": now_unix(),
        "failed_version": st.version,
        "boots": st.boots,
        "outcome": outcome,
    });
    let _ = std::fs::write(dir.join(ROLLBACK_RECORD), record.to_string());
    let _ = std::fs::remove_file(&marker);
    error!(boots = st.boots, version = %st.version,
        "new binary failed to boot repeatedly — auto-rolled back to .prev");
    BootVerdict::RollBack
}

/// Confirm the pending boot as healthy — removes the marker. Idempotent and
/// best-effort; called after server registration and after HEALTHY_SECS of
/// uptime.
pub fn confirm_boot() {
    let Some(dir) = binary_dir() else { return };
    let marker = dir.join(PENDING_MARKER);
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        info!("update boot confirmed healthy — watchdog cleared");
    }
}

/// Write the pending marker right after a successful swap.
fn write_pending_boot(dir: &std::path::Path, version: &str) {
    let st = PendingBoot {
        boots: 0,
        version: version.into(),
        applied_at: now_unix(),
    };
    if let Err(e) = std::fs::write(
        dir.join(PENDING_MARKER),
        serde_json::to_string(&st).unwrap_or_default(),
    ) {
        warn!(error = %e, "could not write boot marker — watchdog disabled for this update");
    }
}

/// Watchdog state for the local status API.
pub fn watchdog_status() -> serde_json::Value {
    let Some(dir) = binary_dir() else {
        return serde_json::json!({"pending": null, "last_rollback": null});
    };
    let pending = std::fs::read_to_string(dir.join(PENDING_MARKER))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let last = std::fs::read_to_string(dir.join(ROLLBACK_RECORD))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    serde_json::json!({"pending": pending, "last_rollback": last})
}

/// Pre-swap smoke test: the staged binary must exec and exit 0 on
/// `--version` within a few seconds. Catches corrupt artifacts, wrong-arch
/// builds and missing runtime deps *before* touching the live binary —
/// failures here leave the install untouched.
async fn smoke_test(staged: &std::path::Path) -> Result<()> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(staged)
            .arg("--version")
            .output(),
    )
    .await
    .context("staged binary --version timed out")?
    .context("staged binary failed to execute")?;
    if !out.status.success() {
        bail!("staged binary --version exited {}", out.status);
    }
    let ver = String::from_utf8_lossy(&out.stdout);
    debug!(out = %ver.trim(), "staged binary smoke test passed");
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
/// Prefers `updater.server_base` (pushed by the server or set at install);
/// otherwise derives it from the gRPC `server_url`. Derivation only knows
/// the default port pair (:50051→:8080) — custom gRPC ports cannot be
/// mapped and fall back to the raw URL, which is why setups should always
/// write an explicit `server_base`.
pub fn manifest_base(cfg: &UpdaterConfig, server_url: &str) -> String {
    if !cfg.server_base.is_empty() {
        return cfg.server_base.trim_end_matches('/').into();
    }
    let u = server_url.trim_end_matches('/');
    if u.is_empty() {
        return String::new();
    }
    // gRPC agent ports → HTTP dashboard :8080 (LOOM defaults). Works
    // with or without an explicit scheme on server_url.
    for port in [":50051", ":50052", ":50053", ":50054"] {
        if let Some(host) = u.strip_suffix(port) {
            return format!("{host}:8080");
        }
    }
    // Bare host:port without scheme still parses in reqwest as a valid
    // relative-looking URL failure — normalize to http:// so the error is
    // a clean connect error, not a URL parse error.
    if !u.contains("://") {
        return format!("http://{u}");
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

/// Self-update preconditions independent of the network: environment shapes
/// where a binary swap is either impossible or the wrong mechanism.
/// Checked *before* downloading so failures are cheap and explicit.
fn ensure_self_update_supported() -> Result<()> {
    // Windows locks the running .exe — an in-place rename always fails.
    if cfg!(windows) {
        bail!("self-update is not supported on Windows; install the new release manually");
    }
    if in_container() && !can_self_update() {
        bail!("binary not on a writable path — run via the image entrypoint (volume-backed binary) or pull a newer image");
    }
    Ok(())
}

/// Whether this process can atomically swap its own binary: always true
/// outside containers; inside a container only when the exe sits on a
/// writable path (the image entrypoint runs it from the persistent state
/// volume — the probe is a real write test, not a path guess). The image
/// layer itself is discarded on recreate, so an unwritable exe means the
/// deployment must be updated by pulling a newer image.
pub(crate) fn can_self_update() -> bool {
    if cfg!(windows) {
        return false;
    }
    if !in_container() {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    let probe = dir.join(".loom-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Best-effort container detection (Docker, Podman, k8s pods, …).
pub(crate) fn in_container() -> bool {
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        return true;
    }
    std::fs::read_to_string("/proc/1/cgroup")
        .map(|c| {
            c.contains("docker")
                || c.contains("kubepods")
                || c.contains("containerd")
                || c.contains("podman")
        })
        .unwrap_or(false)
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
    // Manifest base candidates: the pushed/explicit server_base first, then
    // the base derived from the working gRPC endpoint — a client with a live
    // session provably reaches the server there even when the public
    // hostname is unreachable (split DNS, tunnel down).
    let mut bases = Vec::new();
    let primary = manifest_base(cfg, server_url);
    if !primary.is_empty() {
        bases.push(primary);
    }
    if !cfg.server_base.is_empty() {
        let mut derived = cfg.clone();
        derived.server_base.clear();
        let alt = manifest_base(&derived, server_url);
        if !alt.is_empty() && !bases.contains(&alt) {
            bases.push(alt);
        }
    }
    if bases.is_empty() {
        bail!("no manifest server base configured");
    }
    let mut last_err = None;
    let mut checked = None;
    for base in &bases {
        match check_for_update(base, &cfg.channel, arch(), current).await {
            Ok(r) => {
                checked = Some(r);
                break;
            }
            Err(e) => {
                warn!(base = %base, error = %e, "manifest fetch failed — trying next base");
                last_err = Some(e);
            }
        }
    }
    let (st, mf) = match checked {
        Some(r) => r,
        None => return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("manifest request failed"))),
    };
    let Some(mf) = mf else { return Ok(st) };
    if !st.update_available || !(force || cfg.auto_apply) {
        return Ok(st);
    }
    // The release demands a newer wire protocol than this build speaks —
    // installing it would break the client↔server session.
    if mf.min_version_protocol > loom_protocol::limits::PROTOCOL_VERSION {
        bail!(
            "release {} requires protocol v{} but this build speaks v{} — update the server first",
            mf.version,
            mf.min_version_protocol,
            loom_protocol::limits::PROTOCOL_VERSION
        );
    }
    ensure_self_update_supported()?;
    // Stage next to the running binary: guaranteed same filesystem (rename is
    // atomic across the swap) and writable wherever a self-updatable install
    // lives — the config dir is NOT a safe choice (systemd mounts it
    // read-only under ProtectSystem=strict). `stage_dir` is only a fallback.
    let stage_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| stage_dir.to_path_buf());
    let staged = download_and_stage(&mf, &public_key(cfg), &stage_dir).await?;
    // Exec-validity check before the swap — a corrupt or wrong-arch artifact
    // bails here with the live binary untouched.
    if let Err(e) = smoke_test(&staged).await {
        let _ = std::fs::remove_file(&staged);
        return Err(e.context("staged binary failed smoke test — aborting update"));
    }
    info!(version = %mf.version, "update staged; swapping binary and restarting");
    apply_staged(&staged)?; // keeps a .prev backup for manual rollback
                            // Arm the boot watchdog: next start(s) must prove healthy or the
                            // watchdog restores .prev automatically after MAX_FAILED_BOOTS.
    write_pending_boot(&stage_dir, &mf.version);
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

    #[test]
    fn swap_keeps_prev_and_rolls_back() {
        let dir = std::env::temp_dir().join(format!("loom-upd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("loom-client");
        let staged = dir.join("loom-client.new");
        std::fs::write(&current, b"old-binary").unwrap();
        std::fs::write(&staged, b"new-binary").unwrap();

        let prev = apply_staged_to(&staged, &current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new-binary");
        assert_eq!(std::fs::read(&prev).unwrap(), b"old-binary");
        assert!(!staged.exists());

        rollback_at(&current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"old-binary");
        assert!(!prev.exists());
        assert!(rollback_at(&current).is_err()); // nothing left to restore

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn watchdog_counts_boots_then_rolls_back() {
        let dir = std::env::temp_dir().join(format!("loom-wd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("loom-client");
        std::fs::write(&current, b"new-binary").unwrap();
        std::fs::write(current.with_extension("prev"), b"old-binary").unwrap();

        // No marker → clean.
        assert!(matches!(boot_check(&dir, &current), BootVerdict::Clean));

        // Arm the watchdog as the swap path would.
        write_pending_boot(&dir, "9.9.9");
        for expected in 1..=MAX_FAILED_BOOTS {
            match boot_check(&dir, &current) {
                BootVerdict::Pending { boots } => assert_eq!(boots, expected),
                _ => panic!("boot {expected} should be pending"),
            }
            // Still the new binary while pending.
            assert_eq!(std::fs::read(&current).unwrap(), b"new-binary");
        }
        // One more start than allowed → rollback executes.
        assert!(matches!(boot_check(&dir, &current), BootVerdict::RollBack));
        assert_eq!(std::fs::read(&current).unwrap(), b"old-binary");
        assert!(!dir.join(PENDING_MARKER).exists());
        let rec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(ROLLBACK_RECORD)).unwrap())
                .unwrap();
        assert_eq!(rec["failed_version"], "9.9.9");
        assert_eq!(rec["outcome"], "restored .prev binary");
        // After rollback the next start is clean.
        assert!(matches!(boot_check(&dir, &current), BootVerdict::Clean));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn watchdog_healthy_boot_clears_pending() {
        let dir = std::env::temp_dir().join(format!("loom-wd2-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("loom-client");
        std::fs::write(&current, b"new-binary").unwrap();
        write_pending_boot(&dir, "1.2.3");
        assert!(matches!(
            boot_check(&dir, &current),
            BootVerdict::Pending { boots: 1 }
        ));
        // Healthy boot confirmed (registration or uptime) → marker gone.
        let marker = dir.join(PENDING_MARKER);
        std::fs::remove_file(&marker).unwrap();
        assert!(matches!(boot_check(&dir, &current), BootVerdict::Clean));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn smoke_test_accepts_good_and_rejects_bad() {
        let dir = std::env::temp_dir().join(format!("loom-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A real executable that exits 0 → pass; one exiting 1 → fail.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let good = dir.join("good");
            std::fs::write(&good, "#!/bin/sh\necho v1\nexit 0\n").unwrap();
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(smoke_test(&good).await.is_ok());
            let bad = dir.join("bad");
            std::fs::write(&bad, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(smoke_test(&bad).await.is_err());
        }
        // Non-executable file → cannot exec → fail.
        let junk = dir.join("junk");
        std::fs::write(&junk, b"not a binary").unwrap();
        assert!(smoke_test(&junk).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
