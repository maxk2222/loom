use anyhow::Result;
use loom_core::config::ServerConfig;
use std::path::Path;

pub fn load() -> Result<ServerConfig> {
    // Priority: LOOM_CONFIG file → env vars → defaults.
    let mut cfg = if let Ok(p) = std::env::var("LOOM_CONFIG") {
        let raw = std::fs::read_to_string(Path::new(&p))?;
        toml::from_str::<ServerConfig>(&raw)?
    } else {
        ServerConfig::default()
    };
    if let Ok(v) = std::env::var("LOOM_HTTP_BIND") {
        cfg.http_bind = v;
    }
    if let Ok(v) = std::env::var("LOOM_GRPC_BIND") {
        cfg.grpc_bind = v;
    }
    if let Ok(v) = std::env::var("LOOM_DATABASE_URL") {
        cfg.database_url = v;
    }
    if let Ok(v) = std::env::var("LOOM_REDIS_URL") {
        cfg.redis_url = v;
    }
    if let Ok(v) = std::env::var("LOOM_REDIS_PREFIX") {
        cfg.redis_prefix = v;
    }
    if let Ok(v) = std::env::var("LOOM_DATA_DIR") {
        cfg.data_dir = v;
    }
    if let Ok(v) = std::env::var("LOOM_DASHBOARD_DIR") {
        cfg.dashboard_dir = v;
    }
    Ok(cfg)
}
