use loom_server::*;
use axum::routing::get;
use clap::Parser;
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
    let usage = usage::UsageWriter::new(pool.clone(), cfg.history_retention_days);
    let auth = Arc::new(auth::Auth {
        db: pool.clone(),
        rt: rt.clone(),
    });
    let events = events::EventBus::new();
    let settings = settings::Settings {
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
            b = b.tls_config(tonic::transport::ServerTlsConfig::new()
                .identity(tonic::transport::Identity::from_pem(cert, key))).unwrap();
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
    });
    let adm = admin::router(admin::AdminState {
        db: pool.clone(),
        auth: auth.clone(),
        registry: registry.clone(),
        models: models.clone(),
        scheduler: sched.clone(),
        settings: settings.clone(),
        events: events.clone(),
    });
    let pool2 = pool.clone();
    let rt2 = rt.clone();
    let app = axum::Router::new()
        .merge(gw)
        .merge(adm)
        .nest("/ui", dashboard_static::router())
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(move || {
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
                if db_ok && rd_ok { "ready" } else { "not ready" }
            }
        }))
        .route("/", get(|| async { axum::response::Redirect::permanent("/ui/") }))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let http_addr: SocketAddr = cfg.http_bind.parse()?;
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    info!(%http_addr, "HTTP listening (v1 + api + ui)");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
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
        tracing_subscriber::fmt().with_env_filter(filter).json().init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
