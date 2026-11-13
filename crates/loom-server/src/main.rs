use anyhow::Context;
use axum::response::IntoResponse;
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
    // tonic + reqwest pull in both rustls crypto providers — pick one
    // explicitly or rustls panics when building the TLS config.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    init_tracing();
    let args = Args::parse();
    let cfg = config::load()?;

    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    info!("postgres connected + migrated");

    let rt = redisx::Rt::connect(&cfg.redis_url, &cfg.redis_prefix).await?;
    // Single-owner guard: a second loom-server on the same Redis namespace
    // must not wipe the live instance's runtime state mid-flight. Acquire a
    // short-lived owner lock; only the owner may reset, and it keeps the
    // lease renewed while it runs.
    let instance_lock = rt.k("instance:owner");
    let instance_token = loom_core::ids::opaque_token(16);
    {
        let mut c = rt.conn.clone();
        let got: Option<String> = redis::cmd("SET")
            .arg(&instance_lock)
            .arg(&instance_token)
            .arg("PX")
            .arg(30_000)
            .arg("NX")
            .query_async(&mut c)
            .await
            .unwrap_or(None);
        if got.is_some() {
            rt.reset_runtime().await?; // boot reconciliation: no stale slots
            info!("redis connected, runtime state reset (instance owner)");
            // Renew while running so a stale lock cannot let a peer reset
            // underneath us; CAS on the token so only the owner extends.
            let rt2 = rt.clone();
            let lock_key = instance_lock.clone();
            let token = instance_token.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
                loop {
                    tick.tick().await;
                    let mut c = rt2.conn.clone();
                    let _: i64 = redis::Script::new(
                        "if redis.call('GET', KEYS[1]) == ARGV[1] \
                         then return redis.call('PEXPIRE', KEYS[1], 30000) \
                         else return 0 end",
                    )
                    .key(&lock_key)
                    .arg(&token)
                    .invoke_async(&mut c)
                    .await
                    .unwrap_or(0);
                }
            });
        } else {
            // Another live server owns this Redis namespace. Serve anyway —
            // after the ownership-aware queue/dispatch changes, multi-instance
            // degrades gracefully instead of corrupting — but warn loudly.
            tracing::warn!(
                "another loom-server owns this Redis namespace — starting without \
                 runtime reset (multi-instance: shared queue, per-instance waiters)"
            );
            info!("redis connected (peer instance already owns runtime state)");
        }
    }

    let sessions = session::SessionManager::new();
    let models = model_registry::ModelRegistry::new(pool.clone());
    models.reload().await?;
    let registry = client_registry::ClientRegistry {
        db: pool.clone(),
        rt: rt.clone(),
        models: models.clone(),
    };
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
                for (cid, sid) in sess.connected_ids().await {
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
                        match reg.client_offline(&cid, &sid).await {
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
                        sess.unregister(&cid, &sid).await;
                        // Kill the stale control task itself: without a kick
                        // it keeps running, keeps sending capacity reports,
                        // and can mark deployments available again while no
                        // longer registered — split-brain routing.
                        sess.kick(&cid).await;
                        sch.wake();
                    }
                }
            }
        });
    }
    // Model-registry reconcile: admin changes reload eagerly, but out-of-band
    // writers (manual SQL, migrations, restored backups) would otherwise
    // leave phantom members in the routing snapshot until restart. A slow
    // periodic reload makes the snapshot eventually consistent.
    {
        let m = models.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                if let Err(e) = m.reload().await {
                    tracing::warn!(error = %e, "model registry reconcile reload failed");
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
    // Internal CA for client-cert issuance — always present so enrollment/
    // renewal CSRs can be signed; the strict listener only *requires* client
    // certs when mTLS is enabled (LOOM_MTLS or explicit client_ca_path).
    let client_ca = tls_ca::ClientCa::load_or_create(&cfg.data_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "internal CA unavailable — mTLS issuance disabled");
        None
    });
    let svc = grpc_service::AgentSvc {
        sessions: sessions.clone(),
        registry: registry.clone(),
        scheduler: sched.clone(),
        client_ca: client_ca.clone(),
        listener: "mtls",
    };
    let grpc_addr: SocketAddr = cfg.grpc_bind.parse()?;
    let mut grpc_tls = cfg.grpc_tls.clone();
    // LOOM_MTLS=1: require client certs signed by the internal CA when no
    // explicit client_ca_path was configured.
    let mtls_on = std::env::var("LOOM_MTLS").is_ok_and(|v| v != "0");
    if mtls_on {
        if let Some(t) = grpc_tls.as_mut() {
            if t.client_ca_path.is_none() {
                if let Some(ca) = &client_ca {
                    t.client_ca_path = Some(ca.dir().join("ca.crt").to_string_lossy().into());
                }
            }
        }
    }
    // Shared service instance — a second (plaintext) listener during a TLS
    // migration serves the same AgentSvc.
    let svc = std::sync::Arc::new(svc);

    // TLS config built in the main task — certificate/path mistakes must fail
    // startup loudly, not panic inside a spawned task while HTTP keeps
    // serving a half-dead server.
    // agent_cfg carries client_ca_root (mTLS); bootstrap_cfg is server-auth
    // only — cert issuance must not require a client cert it is issuing.
    let (agent_tls_cfg, bootstrap_tls_cfg) = if let Some(t) = grpc_tls.as_ref() {
        let cert = std::fs::read(&t.cert_path).context("read grpc tls cert")?;
        let key = std::fs::read(&t.key_path).context("read grpc tls key")?;
        let mut agent_cfg = tonic::transport::ServerTlsConfig::new().identity(
            tonic::transport::Identity::from_pem(cert.clone(), key.clone()),
        );
        if let Some(ca) = &t.client_ca_path {
            let pem = std::fs::read(ca).context("read grpc client ca")?;
            agent_cfg = agent_cfg.client_ca_root(tonic::transport::Certificate::from_pem(pem));
        }
        let boot_cfg = tonic::transport::ServerTlsConfig::new()
            .identity(tonic::transport::Identity::from_pem(cert, key));
        (Some(agent_cfg), Some(boot_cfg))
    } else {
        (None, None)
    };

    // Listener failures must not be swallowed: a listener task exiting takes
    // the whole process down (see the select at the bottom of main).
    let mut listener_tasks = tokio::task::JoinSet::new();
    {
        let svc = svc.clone();
        let tls_on = agent_tls_cfg.is_some();
        // Stream fan-out bound per control connection — a rogue client
        // cannot exhaust server resources through one session. Must stay
        // comfortably above a client's legitimate inflight count: every
        // dispatched request occupies its own RequestData stream — a cap
        // below that resets the whole control session under load bursts.
        let mut b = tonic::transport::Server::builder().concurrency_limit_per_connection(512);
        if let Some(tls_cfg) = agent_tls_cfg.clone() {
            b = b.tls_config(tls_cfg).context("grpc agent tls config")?;
        }
        info!(%grpc_addr, tls = tls_on, "gRPC agent endpoint listening");
        listener_tasks.spawn(async move {
            b.add_service(grpc_service::server((*svc).clone()))
                .serve(grpc_addr)
                .await
                .context("grpc agent listener exited")
        });
    }
    // Optional extra plaintext listener — during TLS migrations, or as the
    // loopback target of a trusted tunnel (Cloudflare/cfr: agent hostname →
    // h2c). Without any TLS listener it becomes the only agent path, so it
    // MUST stay on loopback or behind a verified proxy.
    if let Some(plain) = cfg.grpc_plain_bind.clone() {
        if cfg.grpc_tls.is_none() && !plain.starts_with("127.") && !plain.starts_with("[::1]") {
            tracing::warn!(%plain, "plaintext gRPC listener on a non-loopback address without any TLS listener — reachable by any client holding a credential");
        }
        let mut svc = (*svc).clone();
        svc.listener = "plain";
        let addr: SocketAddr = plain.parse().context("parse grpc_plain_bind")?;
        info!(%addr, "gRPC plaintext listener");
        listener_tasks.spawn(async move {
            tonic::transport::Server::builder()
                .concurrency_limit_per_connection(512)
                .add_service(grpc_service::server(svc))
                .serve(addr)
                .await
                .context("grpc plaintext listener exited")
        });
    }
    // Bootstrap listener: TLS server-auth without client-cert — new/renewing
    // clients present their CSR in Hello and get a signed cert back.
    if let (Some(boot), Some(tls_cfg)) =
        (cfg.grpc_bootstrap_bind.clone(), bootstrap_tls_cfg.clone())
    {
        let mut svc = (*svc).clone();
        svc.listener = "bootstrap";
        let addr: SocketAddr = boot.parse().context("parse grpc_bootstrap_bind")?;
        info!(%addr, "gRPC bootstrap listener (cert issuance)");
        listener_tasks.spawn(async move {
            tonic::transport::Server::builder()
                .concurrency_limit_per_connection(512)
                .tls_config(tls_cfg)
                .context("grpc bootstrap tls config")?
                .add_service(grpc_service::server(svc))
                .serve(addr)
                .await
                .context("grpc bootstrap listener exited")
        });
    }

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
        request_deadline_ms: cfg.scheduler.stream_idle_timeout_secs as i64 * 1000,
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
        trusted_proxy: cfg.trusted_proxy,
    });
    let pool2 = pool.clone();
    let rt2 = rt.clone();
    let ca_for_route = client_ca.clone();
    const INSTALL_CLIENT_SH: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/install-client.sh"
    ));
    const DOCKER_ENTRYPOINT_SH: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/docker-entrypoint-client.sh"
    ));
    // WebSocket agent transport on the HTTP listener — the only
    // bidirectional path that survives proxies which buffer request
    // bodies (cloudflared http2 without QUIC). Same auth model as the
    // gRPC listeners: credential in Hello, fencing token in Head.
    let mut ws_svc = (*svc).clone();
    ws_svc.listener = "ws";
    let app = axum::Router::new()
        .merge(gw)
        .merge(adm)
        .merge(ws_agent::router(ws_svc))
        .merge(dashboard_static::router())
        .route("/healthz", get(|| async { "ok" }))
        // Public CA trust anchor for client bootstrap (TOFU): lets setup
        // tooling fetch the CA over HTTP instead of copying files. It only
        // validates server identity — possession grants no access, client
        // certs still need signing and device credentials still gate sessions.
        .route(
            "/api/tls/ca.crt",
            get(move || {
                let ca = ca_for_route.clone();
                async move {
                    match ca {
                        Some(ca) => (
                            [(axum::http::header::CONTENT_TYPE, "application/x-pem-file")],
                            ca.ca_pem().to_string(),
                        )
                            .into_response(),
                        None => (
                            axum::http::StatusCode::NOT_FOUND,
                            "no internal CA (mTLS disabled)",
                        )
                            .into_response(),
                    }
                }
            }),
        )
        // One-command client install: serves deploy/install-client.sh with
        // the public gRPC endpoint (endpoint_hints → public_base) baked in.
        .route(
            "/install-client.sh",
            get({
                let p = pool.clone();
                move || {
                    let p = p.clone();
                    async move {
                        let url = public_server_url(&p).await;
                        let body = if url.is_empty() {
                            INSTALL_CLIENT_SH.to_string()
                        } else {
                            INSTALL_CLIENT_SH.replacen("__LOOM_SERVER_URL__", &url, 1)
                        };
                        (
                            [(
                                axum::http::header::CONTENT_TYPE,
                                "text/x-shellscript; charset=utf-8",
                            )],
                            body,
                        )
                    }
                }
            }),
        )
        // Companion asset for `install-client.sh --docker`: the container
        // entrypoint the generated Dockerfile copies into the image.
        .route(
            "/docker-entrypoint-client.sh",
            get(|| async {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/x-shellscript; charset=utf-8",
                    )],
                    DOCKER_ENTRYPOINT_SH,
                )
            }),
        )
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
        .layer(axum::middleware::from_fn(sec_headers_mw))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // A handler panic must surface as a 500, not an aborted connection
        // that leaves the caller hanging and bookkeeping half-done.
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        // Slow-loris / slot-exhaustion bounds: bounded overall concurrency
        // plus a wall-clock ceiling per request (LLM requests legitimately
        // take minutes — the scheduler deadline still bounds the work,
        // this only caps the HTTP layer's patience for truly stuck calls).
        .layer(tower::limit::ConcurrencyLimitLayer::new(1024))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            // Never tighter than the scheduler's patience: queue wait plus
            // stream idle timeout plus dispatch slack.
            std::time::Duration::from_secs(
                cfg.scheduler.stream_idle_timeout_secs + cfg.scheduler.queue_timeout_secs + 30,
            ),
        ));

    let http_addr: SocketAddr = cfg.http_bind.parse()?;
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    info!(%http_addr, "HTTP listening (v1 + api + ui)");
    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        r = http => r?,
        r = listener_tasks.join_next() => {
            // A listener exiting means the agent channel is gone — a silent
            // half-dead server is worse than a crash restart loop, so die
            // loudly and let the supervisor restart us.
            match r {
                Some(Ok(Err(e))) => anyhow::bail!("{e:#}"),
                Some(Err(je)) => anyhow::bail!("listener task panicked: {je}"),
                Some(Ok(Ok(()))) | None => anyhow::bail!("grpc listener set ended unexpectedly"),
            }
        }
    }
    Ok(())
}

/// Security headers on every HTTP response — dashboard + /api + /v1.
/// CSP pins scripts to 'self'; style-src needs 'unsafe-inline' for React's
/// inline style attributes. HSTS is a no-op on plain HTTP and activates
/// automatically once the surface sits behind TLS.
async fn sec_headers_mw(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, HeaderValue};
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; connect-src 'self'; font-src 'self' data:; \
             object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
        ),
    );
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    res
}

/// Resolve the `--server` default baked into the served install script:
/// first endpoint_hints entry with scope "public", else `public_base`.
/// Empty when nothing public is configured (script then requires --server).
async fn public_server_url(pool: &sqlx::PgPool) -> String {
    let hints: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'endpoint_hints'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    if let Some(arr) = hints.and_then(|v| v.as_array().cloned()) {
        for e in &arr {
            let is_public = e
                .get("scope")
                .and_then(|s| s.as_str())
                .map(|s| s.eq_ignore_ascii_case("public"))
                .unwrap_or(false);
            if is_public {
                if let Some(url) = e.get("url").and_then(|u| u.as_str()) {
                    let url = url.trim();
                    if !url.is_empty() {
                        return url.to_string();
                    }
                }
            }
        }
    }
    let base: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'public_base'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    base.and_then(|v| v.as_str().map(|s| s.trim().to_string()))
        .unwrap_or_default()
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
