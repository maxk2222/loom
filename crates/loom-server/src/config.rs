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
    // gRPC TLS: cert+key enable TLS on grpc_bind; an optional extra
    // plaintext listener (LOOM_GRPC_PLAIN_BIND) keeps legacy clients
    // reachable during a fleet migration.
    match (
        std::env::var("LOOM_GRPC_TLS_CERT"),
        std::env::var("LOOM_GRPC_TLS_KEY"),
    ) {
        (Ok(cert_path), Ok(key_path)) => {
            cfg.grpc_tls = Some(loom_core::config::GrpcTlsConfig {
                cert_path,
                key_path,
                client_ca_path: std::env::var("LOOM_GRPC_TLS_CLIENT_CA").ok(),
            });
        }
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
            tracing::warn!(
                "LOOM_GRPC_TLS_CERT and LOOM_GRPC_TLS_KEY must both be set — TLS disabled"
            );
        }
        _ => {}
    }
    // mTLS client-CA also applies when grpc_tls came from LOOM_CONFIG.
    if let (Some(t), Ok(ca)) = (
        cfg.grpc_tls.as_mut(),
        std::env::var("LOOM_GRPC_TLS_CLIENT_CA"),
    ) {
        t.client_ca_path = Some(ca);
    }
    if let Ok(v) = std::env::var("LOOM_GRPC_PLAIN_BIND") {
        if !v.is_empty() {
            cfg.grpc_plain_bind = Some(v);
        }
    }
    if let Ok(v) = std::env::var("LOOM_GRPC_BOOTSTRAP_BIND") {
        if !v.is_empty() {
            cfg.grpc_bootstrap_bind = Some(v);
        }
    }
    if let Ok(v) = std::env::var("LOOM_TRUST_PROXY") {
        cfg.trusted_proxy = matches!(v.as_str(), "1" | "true" | "yes");
    }
    Ok(cfg)
}
