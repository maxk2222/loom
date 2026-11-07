use axum::routing::get;
use clap::Parser;
use loom_server::*;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "loom-server", version, about = "LOOM central server")]
struct Args {
    /// Metrics listener, e.g. 0.0.0.0:9090
    #[arg(long, env = "LOOM_METRICS_BIND")]
    metrics_bind: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = config::load()?;

    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    info!("postgres connected + migrated");

    let rt = redisx::Rt::connect(&cfg.redis_url, &cfg.redis_prefix).await?;
    rt.reset_runtime().await?; // boot reconciliation: no stale slots
    info!("redis connected, runtime state reset");

    let sessions = session::SessionManager::new();
    let registry = client_registry::ClientRegistry {
        db: pool.clone(),
        rt: rt.clone(),
    };
    let models = model_registry::ModelRegistry::new(pool.clone());
    models.reload().await?;
    let sched = scheduler::Scheduler::new(
        rt.clone(),
        cfg.scheduler.clone(),
        sessions.clone(),
        registry.clone(),
        models.clone(),
    );
    sched.spawn_dispatch_loop();
    // Heartbeat watcher: mark clients offline after 3 missed heartbeats
    // (default hb = 10s → offline after ~30s silence without TCP close).
    {
        let reg = registry.clone();
        let sess = sessions.clone();
        let sch = sched.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                tick.tick().await;
                let mut c = reg.rt.conn.clone();
                for cid in sess.connected_ids().await {
                    let last: Option<String> = redis::cmd("HGET")
                        .arg(reg.rt.k(&format!("client:{cid}")))
                        .arg("last_hb")
                        .query_async(&mut c)
                        .await
                        .unwrap_or(None);
                    let stale = last
                        .and_then(|v| v.parse::<i64>().ok())
                        .map(|t| crate::client_registry::now_ms() - t > 45_000)
                        .unwrap_or(false);
                    if stale {
                        // control loop ended without proper cleanup
                        match reg.client_offline(&cid).await {
                            Ok((requeue, fail)) => {
                                for rid in &requeue {
                                    sch.handle_assign_ack(rid, false, "client_offline").await;
                                    sch.drop_expectation(rid).await;
                                }
                                for rid in &fail {
                                    sch.complete_end(
                                        rid,
                                        scheduler::EndMeta {
                                            status: "failed".into(),
                                            error_code: "client_offline".into(),
                                            ..Default::default()
                                        },
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(client = %cid, error = %e, "hb watcher offline failed")
                            }
                        }
                        sess.unregister(&cid).await;
                        sch.wake();
                    }
                }
            }
        });
    }
    let usage = usage::UsageWriter::new(pool.clone(), cfg.history_retention_days);
    let auth = Arc::new(auth::Auth {
        db: pool.clone(),
        rt: rt.clone(),
        login_throttle: Default::default(),
    });
    let events = events::EventBus::new();
    let settings = settings::Settings {
        database_url: cfg.database_url.clone(),
        db: pool.clone(),
        data_dir: cfg.data_dir.clone(),
    };

    // gRPC agent endpoint
    let svc = grpc_service::AgentSvc {
        sessions: sessions.clone(),
        registry: registry.clone(),
        scheduler: sched.clone(),
    };
    let grpc_addr: SocketAddr = cfg.grpc_bind.parse()?;
    let grpc_tls = cfg.grpc_tls.clone();
    tokio::spawn(async move {
        let mut b = tonic::transport::Server::builder();
        if let Some(t) = grpc_tls {
            let cert = std::fs::read(&t.cert_path).unwrap();
            let key = std::fs::read(&t.key_path).unwrap();
            b = b
                .tls_config(
                    tonic::transport::ServerTlsConfig::new()
                        .identity(tonic::transport::Identity::from_pem(cert, key)),
                )
                .unwrap();
        }
        info!(%grpc_addr, "gRPC agent endpoint listening");
        b.add_service(grpc_service::server(svc))
            .serve(grpc_addr)
            .await
            .ok();
    });

    if let Some(bind) = args.metrics_bind {
        if let Ok(addr) = bind.parse::<SocketAddr>() {
            let _ = metrics_exporter_prometheus::PrometheusBuilder::new()
                .with_http_listener(addr)
                .install();
            info!(%addr, "prometheus metrics");
        }
    }

    // HTTP: /v1 gateway + /api admin + /ui dashboard + health
    let gw = gateway::router(gateway::GatewayState {
        auth: auth.clone(),
        scheduler: sched.clone(),
        models: models.clone(),
        usage: usage.clone(),
        request_deadline_ms: cfg.scheduler.request_deadline_secs as i64 * 1000,
    });
    let adm = admin::router(admin::AdminState {
        db: pool.clone(),
        auth: auth.clone(),
        registry: registry.clone(),
        models: models.clone(),
        scheduler: sched.clone(),
        settings: settings.clone(),
        events: events.clone(),
        sessions: sessions.clone(),
        release_cache: Default::default(),
    });
    let pool2 = pool.clone();
    let rt2 = rt.clone();
    let app = axum::Router::new()
        .merge(gw)
        .merge(adm)
        .merge(dashboard_static::router())
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || {
                let p = pool2.clone();
                let r = rt2.clone();
                async move {
                    let db_ok = sqlx::query("SELECT 1").execute(&p).await.is_ok();
                    let mut c = r.conn.clone();
                    let rd_ok: bool = redis::cmd("PING")
                        .query_async::<String>(&mut c)
                        .await
                        .map(|v| v == "PONG")
                        .unwrap_or(false);
                    if db_ok && rd_ok {
                        "ready"
                    } else {
                        "not ready"
                    }
                }
            }),
        )
        .route(
            "/",
            get(|| async { axum::response::Redirect::permanent("/ui/") }),
        )
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let http_addr: SocketAddr = cfg.http_bind.parse()?;
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    info!(%http_addr, "HTTP listening (v1 + api + ui)");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,loom_server=debug"));
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
