//! loom-client daemon entrypoint.

use clap::Parser;
use loom_client::backend_manager::BackendManager;
use loom_client::config::{default_config_path, ConfigStore};
use loom_client::connector::Connector;
use loom_client::local_api::{self, AppState};
use loom_client::scheduler::LocalScheduler;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "loom-client", version, about = "LOOM workstation agent")]
struct Args {
    /// Config file path (default: platform path or $LOOM_CONFIG).
    #[arg(long, env = "LOOM_CONFIG")]
    config: Option<String>,

    /// Metrics listener (prometheus). e.g. 127.0.0.1:9391
    #[arg(long, env = "LOOM_METRICS_BIND")]
    metrics_bind: Option<String>,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    // Both `aws-lc-rs` and `ring` features land in the dep tree (tonic +
    // reqwest pull both) — rustls refuses to pick a default provider, so
    // install one explicitly before any TLS use.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Peek at the tray flag before deciding where the runtime lives —
    // on macOS the tray needs the main thread.
    #[cfg(feature = "tray")]
    {
        let config_path = Args::parse()
            .config
            .map(Into::into)
            .unwrap_or_else(default_config_path);
        let tray_on = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|raw| toml::from_str::<loom_core::config::ClientConfig>(&raw).ok())
            .map(|c| c.tray.enabled)
            .unwrap_or(false);
        if tray_on {
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                let _ = rt.block_on(async_main());
            });
            return loom_client::tray::run();
        }
    }
    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let args = Args::parse();
    // Boot-failure watchdog: count this start while an update is pending and
    // auto-roll back to .prev after repeated crashes. Runs after Args::parse
    // so --version/--help don't count as boots; before config load so an
    // init crash still increments the counter.
    loom_client::updater::boot_watchdog();
    // A healthy boot is confirmed by server registration (connector) OR by
    // plain uptime — the binary clearly works even when the server is down.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(
            loom_client::updater::HEALTHY_SECS,
        ))
        .await;
        loom_client::updater::confirm_boot();
    });
    let config_path = args
        .config
        .map(Into::into)
        .unwrap_or_else(default_config_path);
    info!(path = %config_path.display(), "starting loom-client");

    let cfg_store = Arc::new(ConfigStore::load(&config_path)?);
    let cfg = cfg_store.get().await;

    let backends = BackendManager::new();
    backends.sync_from_config(&cfg).await;

    let dep_cfgs: Vec<_> = cfg
        .backends
        .iter()
        .flat_map(|b| b.deployments.clone())
        .collect();
    let scheduler = LocalScheduler::new(cfg.sharing_enabled);
    scheduler.sync_deployments(&dep_cfgs);

    let connector = Connector::new(cfg_store.clone(), backends.clone(), scheduler.clone());
    backends.spawn_health_loop(
        scheduler.clone(),
        connector.state().capacity_notify.clone(),
        connector.state().inventory_notify.clone(),
    );

    // Pressure monitor: samples cpu/ram, closes tier-2 gates under load and
    // optionally unloads idle models. Stats feed the dashboard.
    let sys_stats = loom_client::sysmon::spawn(
        cfg_store.clone(),
        scheduler.clone(),
        backends.clone(),
        connector.state().capacity_notify.clone(),
    );

    // Admin notes pushed by the server; persisted next to the config.
    let notes_path = config_path.with_file_name("notes.json");
    let notes = loom_client::notes::load_notes(&notes_path);
    connector.set_notes(notes.clone(), notes_path.clone());

    // Shared history ring: the connector appends tier-2 finishes, the local
    // API appends local finishes; both surface in /local/api/history.
    // Persisted to history.json next to the config so charts survive restarts.
    let history_path = config_path.with_file_name("history.json");
    let history: std::sync::Arc<std::sync::RwLock<std::collections::VecDeque<local_api::FinReq>>> =
        std::sync::Arc::new(std::sync::RwLock::new(local_api::load_history_file(
            &history_path,
        )));
    backends.set_sys_stats(sys_stats.clone()).await;
    connector.set_sys_stats(sys_stats.clone());
    connector.set_history(history.clone());
    {
        let h = history.clone();
        let hp = history_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                local_api::save_history_file(&hp, &h);
            }
        });
    }

    let state = AppState {
        cfg: cfg_store.clone(),
        backends: backends.clone(),
        scheduler: scheduler.clone(),
        connector: connector.clone(),
        started: Instant::now(),
        requests: Default::default(),
        history,
        sessions: Default::default(),
        notes,
        notes_path,
        sys_stats,
    };

    // Auto-updater: periodic manifest check when enabled. Manual/admin
    // triggers go through the same run_once path (local API / ConfigPush).
    {
        let cfg_store = cfg_store.clone();
        let stage_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        tokio::spawn(async move {
            let mut first = true;
            loop {
                let cfg = cfg_store.get().await;
                if cfg.updater.enabled {
                    match loom_client::updater::run_once(
                        &cfg.updater,
                        cfg.primary_server_url(),
                        env!("CARGO_PKG_VERSION"),
                        &stage_dir,
                        false,
                    )
                    .await
                    {
                        Ok(st) if st.update_available => {
                            info!(version = ?st.latest_version, "update available (auto_apply off)")
                        }
                        Ok(_) => {}
                        Err(e) => tracing::debug!(error = %e, "update check failed"),
                    }
                }
                // first tick after a short delay, then every 30 min
                let delay = if first { 60 } else { 30 * 60 };
                first = false;
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
        });
    }

    // Metrics listener (optional).
    if let Some(bind) = args
        .metrics_bind
        .or_else(|| std::env::var("LOOM_METRICS_BIND").ok())
    {
        if let Ok(addr) = bind.parse::<SocketAddr>() {
            match loom_client::telemetry::spawn_metrics_listener(addr).await {
                Ok(_) => info!(%addr, "prometheus metrics listener started"),
                Err(e) => error!(error = %e, "metrics listener failed"),
            }
        }
    }

    // Connector task.
    let conn_task = {
        let c = connector.clone();
        tokio::spawn(async move { c.run().await })
    };

    // Local API listener.
    let bind_addr: SocketAddr = cfg
        .local_api
        .bind
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:8391".parse().unwrap());
    let app = local_api::router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    if !bind_addr.ip().is_loopback()
        && cfg.local_api.api_key.is_empty()
        && cfg.local_api.user_hash.is_empty()
    {
        warn!(
            %bind_addr,
            "local API is reachable from the network with NO authentication — \
             set local_api.api_key or a UI user before exposing it"
        );
    }
    info!(%bind_addr, "local API + UI listening");

    let shutdown = connector.clone();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutdown.shutdown();
    })
    .await?;

    conn_task.abort();
    backends.shutdown();
    info!("loom-client stopped");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,loom_client=debug"));
    let json = std::env::var("LOOM_LOG_JSON")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
