//! Client configuration: TOML file load/save + live config handle.

use anyhow::{Context, Result};
use loom_core::config::ClientConfig;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

pub struct ConfigStore {
    path: PathBuf,
    cfg: RwLock<ClientConfig>,
}

impl ConfigStore {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let cfg = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read config {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?
        } else {
            ClientConfig::default()
        };
        Ok(Self {
            path,
            cfg: RwLock::new(cfg),
        })
    }

    pub async fn get(&self) -> ClientConfig {
        self.cfg.read().await.clone()
    }

    pub async fn update<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut ClientConfig) -> R,
    {
        let mut guard = self.cfg.write().await;
        let r = f(&mut guard);
        self.persist_locked(&guard)?;
        Ok(r)
    }

    /// Persist without invoking a mutation closure.
    /// Directory containing the config file — used for co-located state
    /// (mTLS cert store, local DB).
    pub fn dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub async fn persist(&self) -> Result<()> {
        let guard = self.cfg.read().await;
        self.persist_locked(&guard)
    }

    fn persist_locked(&self, cfg: &ClientConfig) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let toml = toml::to_string_pretty(cfg).context("serialize config")?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml).context("write config tmp")?;
        std::fs::rename(&tmp, &self.path).context("rename config")?;
        // Config holds device credentials and the local API key — owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Default config path resolution: $LOOM_CONFIG or platform default.
pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("LOOM_CONFIG") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Application Support/loom-client/config.toml");
        }
    }
    PathBuf::from("/etc/loom-client/config.toml")
}
