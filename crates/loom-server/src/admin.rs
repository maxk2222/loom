//! Admin REST API (/api/*): dashboard auth, CRUD for all entities, branding,
//! SSE events, stats, config versions, enrollment tokens, scheduler views.

use crate::auth::{Auth, LoginBody, UserCtx};
use crate::client_registry::ClientRegistry;
use crate::events::EventBus;
use crate::model_registry::ModelRegistry;
use crate::scheduler::Scheduler;
use crate::settings::Settings;
use axum::{
    body::Body,
    extract::{FromRequest, Multipart, Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, post, put},
    Json, Router,
};
use loom_core::ids::{opaque_token, sha256_hex};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AdminState {
    pub db: PgPool,
    pub auth: Arc<Auth>,
    pub registry: ClientRegistry,
    pub models: ModelRegistry,
    pub scheduler: Scheduler,
    pub settings: Settings,
    pub events: EventBus,
    pub sessions: Arc<crate::session::SessionManager>,
    /// Release-manifest cache (key → fetched-at + payload), avoids hammering
    /// the GitHub API once per client poll.
    pub release_cache:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Value)>>>,
    /// Trust CF-Connecting-IP / X-Forwarded-* headers (ServerConfig
    /// `trusted_proxy`). Only enable behind a proxy every request passes.
    pub trusted_proxy: bool,
}

/// Real client IP for rate limiting + audit: prefers Cloudflare's
/// guaranteed header, then the first X-Forwarded-For hop, and falls back to
/// the TCP peer. Forwarded headers are honoured ONLY under `trusted_proxy` —
/// otherwise any client could spoof them to reset its rate-limit bucket.
fn client_ip(req: &Request, trusted_proxy: bool) -> Option<std::net::IpAddr> {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    if !trusted_proxy {
        return peer;
    }
    req.headers()
        .get("cf-connecting-ip")
        .or_else(|| req.headers().get("x-forwarded-for"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .or(peer)
}

pub fn router(s: AdminState) -> Router {
    // Sensitive unauthenticated endpoints get a per-IP token bucket on top
    // of the per-account login throttle — blocks raw IP floods of guesses.
    let throttled = Router::new()
        .route("/api/setup/admin", post(setup_admin))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .layer(middleware::from_fn_with_state(s.clone(), ip_rl_mw))
        .with_state(s.clone());
    let public = Router::new()
        .route("/api/branding", get(branding))
        .route("/api/update-check", get(update_check))
        .route("/api/releases/manifest", get(release_manifest))
        .route("/api/branding/logo.{ext}", get(brand_logo))
        .route("/api/setup/status", get(setup_status))
        .merge(throttled)
        .with_state(s.clone());

    let protected = Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/overview", get(overview))
        .route("/api/events", get(events_sse))
        // users
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", put(update_user).delete(delete_user))
        // projects
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            put(update_project).delete(delete_project),
        )
        .route(
            "/api/projects/{id}/models",
            get(get_project_models).put(set_project_models),
        )
        // api keys
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{id}", put(update_key).delete(delete_key))
        .route("/api/keys/{id}/revoke", post(revoke_key))
        // clients
        .route("/api/clients", get(list_clients))
        .route(
            "/api/clients/{id}",
            get(client_detail).delete(delete_client),
        )
        .route("/api/routers", get(list_routers).post(create_router))
        .route(
            "/api/routers/{id}",
            put(update_router).delete(delete_router),
        )
        .route("/api/routers/{id}/test", post(test_router))
        .route("/api/clients/{id}/status", post(client_status))
        .route("/api/clients/{id}/update", post(push_client_update))
        .route("/api/server/update", post(server_update))
        .route("/api/enroll-tokens", post(issue_enroll_token))
        // client notices (notes/briefings pushed to client dashboards)
        .route("/api/notes", get(list_notes).post(create_note))
        .route("/api/notes/{id}", delete(delete_note))
        // deployments
        .route("/api/deployments", get(list_deployments))
        .route(
            "/api/deployments/{id}",
            put(update_deployment).delete(delete_deployment),
        )
        // virtual models
        .route("/api/models", get(list_vms).post(create_vm))
        .route(
            "/api/models/{id}",
            get(vm_detail).put(update_vm).delete(delete_vm),
        )
        // scheduler/queue
        .route("/api/scheduler/state", get(sched_state))
        .route("/api/requests/{id}/cancel", post(cancel_request))
        // monitoring
        .route("/api/history", get(history))
        .route("/api/stats/usage", get(usage_stats))
        .route("/api/stats/series", get(stats_series))
        .route("/api/stats/breakdown", get(stats_breakdown))
        .route("/api/stats/purge-orphans", post(purge_orphan_stats))
        // settings
        .route("/api/settings/{key}", get(get_setting).put(put_setting))
        .route("/api/branding", put(put_branding))
        .route("/api/branding/logo", post(upload_logo))
        .route("/api/config-versions", get(config_versions))
        .route("/api/system", get(system_info))
        .route("/api/system/backup", post(trigger_backup))
        .layer(middleware::from_fn_with_state(s.clone(), session_mw))
        .with_state(s.clone());

    public.merge(protected)
}

// ---------------- auth ----------------

/// Per-IP token bucket: burst 20, refill 2/s. Process-global — protects
/// unauthenticated endpoints from credential sprays before any per-account
/// logic even runs. Complements the (ip,email) login lockout.
fn ip_buckets(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (f64, std::time::Instant)>>
{
    static B: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (f64, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    B.get_or_init(Default::default)
}

async fn ip_rl_mw(
    State(s): State<AdminState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(ip) = client_ip(&req, s.trusted_proxy) {
        let now = std::time::Instant::now();
        let mut map = ip_buckets().lock().unwrap();
        if map.len() > 10_000 {
            map.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 300);
        }
        let (tokens, last) = map.entry(ip).or_insert((20.0, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * 2.0).min(20.0);
        *last = now;
        if *tokens < 1.0 {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        *tokens -= 1.0;
    }
    Ok(next.run(req).await)
}

async fn session_mw(
    State(s): State<AdminState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|p| p.trim().strip_prefix("loom_session="))
                .next()
                .map(|v| v.to_string())
        })
        .unwrap_or_default();
    match s.auth.user_for_token(&token).await {
        Ok(Some(u)) => {
            let mut req = req;
            req.extensions_mut().insert(u);
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

fn session_cookie(token: &str, secure: bool) -> String {
    let sec = if secure { "; Secure" } else { "" };
    format!("loom_session={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age=604800{sec}")
}

/// Whether the session cookie may carry the `Secure` flag: the request arrived
/// over HTTPS (per proxy header, honoured only under `trusted_proxy`) or the
/// operator opted in via env — set LOOM_SECURE_COOKIES=1 when serving the
/// admin API behind TLS.
fn cookie_is_secure(headers: &axum::http::HeaderMap, trusted_proxy: bool) -> bool {
    (trusted_proxy
        && headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https")))
        || std::env::var("LOOM_SECURE_COOKIES")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
}

async fn login(
    State(s): State<AdminState>,
    headers: axum::http::HeaderMap,
    req: Request,
) -> Response {
    let ip = client_ip(&req, s.trusted_proxy)
        .map(|i| i.to_string())
        .unwrap_or_default();
    let secure = cookie_is_secure(&headers, s.trusted_proxy);
    let Json(b): Json<LoginBody> = match Json::from_request(req, &s).await {
        Ok(j) => j,
        Err(e) => return e.into_response(),
    };
    match s.auth.login(&b.email, &b.password, &ip).await {
        Ok(Some((tok, u))) => Response::builder()
            .status(StatusCode::OK)
            .header(header::SET_COOKIE, session_cookie(&tok, secure))
            .body(Body::from(
                serde_json::to_string(
                    &json!({"user": {"id": u.user_id, "role": u.role, "email": u.email}}),
                )
                .unwrap(),
            ))
            .unwrap(),
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn logout(State(s): State<AdminState>, req: Request) -> Response {
    if let Some(t) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|p| p.trim().strip_prefix("loom_session="))
                .next()
        })
        .map(|v| v.to_string())
    {
        let _ = s.auth.logout(&t).await;
    }
    // Also expire the cookie client-side so stale sessions cannot linger.
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::SET_COOKIE,
            "loom_session=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0",
        )
        .body(Body::empty())
        .unwrap()
}

async fn me(req: Request) -> Json<Value> {
    let u = req.extensions().get::<UserCtx>().cloned().unwrap();
    Json(json!({"user": {"id": u.user_id, "role": u.role, "email": u.email}}))
}

async fn setup_status(State(s): State<AdminState>) -> Json<Value> {
    let n = s.auth.admin_count().await.unwrap_or(0);
    Json(json!({"needs_setup": n == 0}))
}

async fn setup_admin(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let email = b.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let pw = b.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let name = b.get("display_name").and_then(|v| v.as_str()).unwrap_or("");
    if pw.len() < 8 || email.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid input"})),
        )
            .into_response();
    }
    match s.auth.create_first_admin(email, pw, name).await {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        _ => StatusCode::CONFLICT.into_response(),
    }
}

// ---------------- overview / events ----------------

async fn overview(State(s): State<AdminState>) -> Json<Value> {
    let clients = s.registry.live_clients().await.unwrap_or_default();
    let online = clients.iter().filter(|c| c["online"] == true).count();
    let vms = s.models.list().await;
    // Live queue depth + in-flight count from the scheduler — history only
    // stores terminal states, so querying it for 'queued'/'streaming'
    // always returns 0.
    let (queued, active) = s.scheduler.live_counts().await;
    let today: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM requests_history WHERE created_at > now() - interval '1 day'",
    )
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);
    Json(json!({
        "clients_total": clients.len(),
        "clients_online": online,
        "virtual_models": vms.len(),
        "queued": queued,
        "inflight": active,
        "requests_24h": today,
    }))
}

async fn events_sse(
    State(s): State<AdminState>,
) -> Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    let mut rx = s.events.subscribe();
    let stream = async_stream::stream! {
        // initial keepalive
        yield Ok(axum::response::sse::Event::default().comment("ok"));
        loop {
            match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
                Ok(Ok(msg)) => {
                    // msg already in SSE wire format "event: x\ndata: {...}"
                    for part in msg.split("\n\n") {
                        if part.is_empty() { continue; }
                        let mut ev = axum::response::sse::Event::default();
                        for line in part.lines() {
                            if let Some(d) = line.strip_prefix("data: ") { ev = ev.data(d); }
                            if let Some(e) = line.strip_prefix("event: ") { ev = ev.event(e); }
                        }
                        yield Ok(ev);
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {
                    yield Ok(axum::response::sse::Event::default().comment("ka"));
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(10)))
}

// ---------------- users ----------------

async fn list_users(State(s): State<AdminState>) -> Json<Value> {
    let rows: Vec<(uuid::Uuid, String, String, String, bool)> =
        sqlx::query_as("SELECT id, email, display_name, role, disabled FROM users ORDER BY email")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    Json(
        json!({"users": rows.iter().map(|(id,e,n,r,d)| json!({"id":id,"email":e,"display_name":n,"role":r,"disabled":d})).collect::<Vec<_>>()}),
    )
}

async fn create_user(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let email = b["email"].as_str().unwrap_or("").to_string();
    let pw = b["password"].as_str().unwrap_or("");
    let role = b["role"].as_str().unwrap_or("viewer");
    let name = b["display_name"].as_str().unwrap_or("");
    if email.is_empty() || pw.len() < 8 || !["admin", "operator", "viewer"].contains(&role) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid input"})),
        )
            .into_response();
    }
    let hash = match Auth::hash_password(pw) {
        Ok(h) => h,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match sqlx::query(
        "INSERT INTO users (email,password_hash,display_name,role) VALUES ($1,$2,$3,$4)",
    )
    .bind(&email)
    .bind(&hash)
    .bind(name)
    .bind(role)
    .execute(&s.db)
    .await
    {
        Ok(_) => {
            audit(&s.db, "user", "create", &email).await;
            Json(json!({"ok":true})).into_response()
        }
        Err(_) => (StatusCode::CONFLICT, Json(json!({"error":"email exists"}))).into_response(),
    }
}

async fn update_user(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Some(role) = b.get("role").and_then(|v| v.as_str()) {
        let _ = sqlx::query("UPDATE users SET role=$2, updated_at=now() WHERE id=$1::uuid")
            .bind(&id)
            .bind(role)
            .execute(&s.db)
            .await;
    }
    if let Some(d) = b.get("disabled").and_then(|v| v.as_bool()) {
        let _ = sqlx::query("UPDATE users SET disabled=$2, updated_at=now() WHERE id=$1::uuid")
            .bind(&id)
            .bind(d)
            .execute(&s.db)
            .await;
    }
    if let Some(pw) = b.get("password").and_then(|v| v.as_str()) {
        if pw.len() >= 8 {
            if let Ok(h) = Auth::hash_password(pw) {
                let _ = sqlx::query(
                    "UPDATE users SET password_hash=$2, updated_at=now() WHERE id=$1::uuid",
                )
                .bind(&id)
                .bind(h)
                .execute(&s.db)
                .await;
            }
        }
    }
    audit(&s.db, "user", "update", &id).await;
    Json(json!({"ok":true})).into_response()
}

async fn delete_user(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    let _ = sqlx::query("DELETE FROM users WHERE id=$1::uuid AND role != 'admin'")
        .bind(&id)
        .execute(&s.db)
        .await;
    audit(&s.db, "user", "delete", &id).await;
    Json(json!({"ok":true})).into_response()
}

// ---------------- projects ----------------

async fn list_projects(State(s): State<AdminState>) -> Json<Value> {
    let rows: Vec<(uuid::Uuid, String, String, i32, i32, bool)> = sqlx::query_as(
        "SELECT id, slug, name, priority_class, weight, enabled FROM projects ORDER BY slug",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    Json(
        json!({"projects": rows.iter().map(|(id,s,n,p,w,e)| json!({"id":id,"slug":s,"name":n,"priority_class":p,"weight":w,"enabled":e})).collect::<Vec<_>>()}),
    )
}

async fn create_project(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let slug = b["slug"].as_str().unwrap_or("");
    let name = b["name"].as_str().unwrap_or("");
    if slug.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"slug required"})),
        )
            .into_response();
    }
    match sqlx::query("INSERT INTO projects (slug,name,priority_class,weight) VALUES ($1,$2,$3,$4)")
        .bind(slug)
        .bind(name)
        .bind(b["priority_class"].as_i64().unwrap_or(1) as i32)
        .bind(b["weight"].as_i64().unwrap_or(1) as i32)
        .execute(&s.db)
        .await
    {
        Ok(_) => {
            audit(&s.db, "project", "create", slug).await;
            Json(json!({"ok":true})).into_response()
        }
        Err(_) => (StatusCode::CONFLICT, Json(json!({"error":"slug exists"}))).into_response(),
    }
}

async fn update_project(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let _ = sqlx::query(
        "UPDATE projects SET name=COALESCE($2,name), priority_class=COALESCE($3,priority_class), weight=COALESCE($4,weight), enabled=COALESCE($5,enabled) WHERE id=$1::uuid")
        .bind(&id)
        .bind(b.get("name").and_then(|v| v.as_str()))
        .bind(b.get("priority_class").and_then(|v| v.as_i64()).map(|v| v as i32))
        .bind(b.get("weight").and_then(|v| v.as_i64()).map(|v| v as i32))
        .bind(b.get("enabled").and_then(|v| v.as_bool()))
        .execute(&s.db).await;
    audit(&s.db, "project", "update", &id).await;
    Json(json!({"ok":true})).into_response()
}

async fn delete_project(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    let _ = sqlx::query("DELETE FROM projects WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    audit(&s.db, "project", "delete", &id).await;
    Json(json!({"ok":true})).into_response()
}

async fn get_project_models(State(s): State<AdminState>, Path(id): Path<String>) -> Json<Value> {
    let ids: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT virtual_model_id FROM project_models WHERE project_id=$1::uuid")
            .bind(&id)
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    Json(json!({"virtual_model_ids": ids.iter().map(|(i,)| i.to_string()).collect::<Vec<_>>()}))
}

async fn set_project_models(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let ids: Vec<String> = b["virtual_model_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut tx = match s.db.begin().await {
        Ok(t) => t,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let _ = sqlx::query("DELETE FROM project_models WHERE project_id=$1::uuid")
        .bind(&id)
        .execute(&mut *tx)
        .await;
    for vm in &ids {
        let _ = sqlx::query(
            "INSERT INTO project_models (project_id, virtual_model_id) VALUES ($1::uuid,$2::uuid)",
        )
        .bind(&id)
        .bind(vm)
        .execute(&mut *tx)
        .await;
    }
    let _ = tx.commit().await;
    audit(&s.db, "project", "set_models", &id).await;
    Json(json!({"ok":true})).into_response()
}

// ---------------- api keys ----------------

async fn list_keys(State(s): State<AdminState>) -> Json<Value> {
    type KeyRow = (
        uuid::Uuid,
        uuid::Uuid,
        String,
        String,
        i32,
        i32,
        i32,
        bool,
        serde_json::Value,
        serde_json::Value,
    );
    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT id, project_id, name, prefix, rpm_limit, tpm_limit, max_concurrent, (revoked_at IS NULL) AS active, scopes, allowed_models FROM api_keys ORDER BY created_at DESC",
    ).fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"keys": rows.iter().map(|(id,p,n,pre,rpm,tpm,mc,act,sc,am)| json!({"id":id,"project_id":p,"name":n,"prefix":pre,"rpm_limit":rpm,"tpm_limit":tpm,"max_concurrent":mc,"active":act,"scopes":sc,"allowed_models":am})).collect::<Vec<_>>()}),
    )
}

async fn create_key(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let project = b["project_id"].as_str().unwrap_or("");
    if project.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"project_id required"})),
        )
            .into_response();
    }
    let token = format!("sk-{}", opaque_token(24));
    let hash = sha256_hex(&token);
    let prefix = token.chars().take(10).collect::<String>();
    let res = sqlx::query(
        "INSERT INTO api_keys (project_id,name,key_hash,prefix,scopes,rpm_limit,tpm_limit,max_concurrent,allowed_models)
         VALUES ($1::uuid,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id::text")
        .bind(project)
        .bind(b["name"].as_str().unwrap_or(""))
        .bind(&hash).bind(&prefix)
        .bind(b.get("scopes").cloned().unwrap_or_else(|| json!(["chat","completions","embeddings","models"])))
        .bind(b["rpm_limit"].as_i64().unwrap_or(60) as i32)
        .bind(b["tpm_limit"].as_i64().unwrap_or(100000) as i32)
        .bind(b["max_concurrent"].as_i64().unwrap_or(4) as i32)
        .bind(b.get("allowed_models").cloned().unwrap_or_else(|| json!([])))
        .fetch_one(&s.db).await;
    match res {
        Ok(row) => {
            use sqlx::Row;
            let id: String = row.get(0);
            audit(&s.db, "api_key", "create", &id).await;
            Json(json!({"ok":true,"id":id,"token":token})).into_response()
        }
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid project"})),
        )
            .into_response(),
    }
}

async fn revoke_key(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    let _ = sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    audit(&s.db, "api_key", "revoke", &id).await;
    Json(json!({"ok":true})).into_response()
}

/// Hard-delete an API key. Unlike revoke, the row is removed entirely —
/// requests_history/usage_counters keep their (dangling) UUID references;
/// the LEFT JOINs there already tolerate missing keys.
async fn delete_key(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    let r = sqlx::query("DELETE FROM api_keys WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    match r {
        Ok(res) if res.rows_affected() > 0 => {
            audit(&s.db, "api_key", "delete", &id).await;
            Json(json!({"ok":true})).into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"key not found"})),
        )
            .into_response(),
    }
}

/// Update a key's label, scopes, rate/token/concurrency limits and per-key
/// model allowlist (allowed_models [] = inherit project grants, ["*"] = all).
async fn update_key(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    // Partial update: only fields present in the body change; the rest keep
    // their stored values (avoids clobbering limits/models on renames etc).
    let cur: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('name',name,'rpm_limit',rpm_limit,'tpm_limit',tpm_limit,
                'max_concurrent',max_concurrent,'scopes',scopes,'allowed_models',allowed_models)
         FROM api_keys WHERE id=$1::uuid AND revoked_at IS NULL",
    )
    .bind(&id)
    .fetch_optional(&s.db)
    .await
    .unwrap_or(None);
    let Some(cur) = cur else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"key not found"})),
        )
            .into_response();
    };
    let get = |k: &str| b.get(k).cloned().unwrap_or_else(|| cur[k].clone());
    let r = sqlx::query(
        "UPDATE api_keys SET name=$2, rpm_limit=$3, tpm_limit=$4, max_concurrent=$5,
                scopes=$6, allowed_models=$7 WHERE id=$1::uuid AND revoked_at IS NULL",
    )
    .bind(&id)
    .bind(get("name").as_str().unwrap_or_default().to_string())
    .bind(get("rpm_limit").as_i64().unwrap_or(60) as i32)
    .bind(get("tpm_limit").as_i64().unwrap_or(100000) as i32)
    .bind(get("max_concurrent").as_i64().unwrap_or(4) as i32)
    .bind(get("scopes"))
    .bind(get("allowed_models"))
    .execute(&s.db)
    .await;
    match r {
        Ok(res) if res.rows_affected() > 0 => {
            audit(&s.db, "api_key", "update", &id).await;
            Json(json!({"ok":true})).into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"key not found"})),
        )
            .into_response(),
    }
}

// ---------------- clients ----------------

async fn list_clients(State(s): State<AdminState>) -> Json<Value> {
    Json(json!({"clients": s.registry.live_clients().await.unwrap_or_default()}))
}

async fn client_detail(State(s): State<AdminState>, Path(id): Path<String>) -> Json<Value> {
    let deps: Vec<Value> = sqlx::query_scalar::<_, String>(
        "SELECT id::text FROM deployments WHERE client_id=$1::uuid",
    )
    .bind(&id)
    .fetch_all(&s.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|d| json!(d))
    .collect();
    let live = s.registry.live_clients().await.unwrap_or_default();
    let c = live
        .into_iter()
        .find(|c| c["id"].as_str() == Some(id.as_str()))
        .unwrap_or(json!({}));
    Json(json!({"client": c, "deployments": deps}))
}

async fn client_status(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let status = b["status"].as_str().unwrap_or("");
    if !["active", "suspended", "pending"].contains(&status) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid status"})),
        )
            .into_response();
    }
    let _ = sqlx::query("UPDATE clients SET status=$2 WHERE id=$1::uuid")
        .bind(&id)
        .bind(status)
        .execute(&s.db)
        .await;
    // Suspension must act on the live session, not just the next reconnect:
    // kicking drops the control stream through the normal disconnect path,
    // so in-flight work is requeued/failed and the client is marked offline.
    if status != "active" {
        s.sessions.kick(&id).await;
    }
    audit(&s.db, "client", "status", &format!("{id}:{status}")).await;
    Json(json!({"ok":true})).into_response()
}

/// Permanent removal: kick the live session first so in-flight work
/// requeues/fails cleanly, then drop the row — deployments, VM/router
/// memberships and client notes cascade with it. The device credential
/// dies with the row, so the workstation's next reconnect is rejected.
async fn delete_client(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid client id"})),
        )
            .into_response();
    }
    s.sessions.kick(&id).await;
    match sqlx::query("DELETE FROM clients WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            // Cascading member rows die with the deployments — drop the
            // cached VM member lists immediately instead of waiting for
            // the 60s reconcile tick.
            let _ = s.models.reload().await;
            audit(&s.db, "client", "delete", &id).await;
            Json(json!({"ok":true})).into_response()
        }
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"client not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

async fn issue_enroll_token(State(s): State<AdminState>, req: Request) -> Response {
    let user = req.extensions().get::<UserCtx>().cloned();
    let body = axum::body::to_bytes(req.into_body(), 8192)
        .await
        .unwrap_or_default();
    let b: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let token = format!("enroll_{}", opaque_token(24));
    let res = sqlx::query(
        "INSERT INTO enroll_tokens (token_hash,label,max_uses,expires_at,created_by)
         VALUES ($1,$2,$3, now() + ($4 || ' hours')::interval, $5::uuid) RETURNING id::text",
    )
    .bind(sha256_hex(&token))
    .bind(b["label"].as_str().unwrap_or(""))
    .bind(b["max_uses"].as_i64().unwrap_or(1) as i32)
    .bind(b["expires_hours"].as_i64().unwrap_or(24).to_string())
    .bind(user.and_then(|u| uuid::Uuid::parse_str(&u.user_id).ok()))
    .fetch_one(&s.db)
    .await;
    match res {
        Ok(row) => {
            use sqlx::Row;
            audit(&s.db, "enroll_token", "issue", "").await;
            Json(json!({"ok":true,"id":row.get::<String,_>(0),"token":token})).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ---------------- client notices (notes/briefings) ----------------

async fn list_notes(State(s): State<AdminState>) -> Json<Value> {
    type Row = (
        uuid::Uuid,
        Option<uuid::Uuid>,
        Option<String>,
        String,
        String,
        String,
        bool,
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT n.id, n.client_id, c.name AS client_name, n.title, n.body,
                n.severity, n.persistent, n.expires_at, n.created_at
         FROM client_notes n LEFT JOIN clients c ON c.id = n.client_id
         ORDER BY n.created_at DESC LIMIT 200",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    Json(json!({
        "notes": rows.iter().map(|(id,cid,cn,t,b,sev,p,exp,ca)| json!({
            "id": id, "client_id": cid, "client_name": cn,
            "title": t, "body": b, "severity": sev, "persistent": p,
            "expires_at": exp, "created_at": ca,
        })).collect::<Vec<_>>()
    }))
}

async fn create_note(State(s): State<AdminState>, req: Request) -> Response {
    let user = req.extensions().get::<UserCtx>().cloned();
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let b: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let title = b["title"].as_str().unwrap_or("").trim().to_string();
    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"title required"})),
        )
            .into_response();
    }
    let severity = match b["severity"].as_str().unwrap_or("info") {
        "warning" => "warning",
        "critical" => "critical",
        _ => "info",
    };
    // Empty/absent client_id → broadcast (NULL).
    let client_id = b["client_id"].as_str().unwrap_or("").trim().to_string();
    if !client_id.is_empty() {
        if uuid::Uuid::parse_str(&client_id).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid client_id"})),
            )
                .into_response();
        }
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM clients WHERE id=$1::uuid)")
                .bind(&client_id)
                .fetch_one(&s.db)
                .await
                .unwrap_or(false);
        if !exists {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"client not found"})),
            )
                .into_response();
        }
    }
    let res = sqlx::query(
        "INSERT INTO client_notes (client_id,title,body,severity,persistent,expires_at,created_by)
         VALUES (NULLIF($1,'')::uuid,$2,$3,$4,$5,
                 CASE WHEN $6 > 0 THEN now() + ($6 || ' hours')::interval END,
                 $7) RETURNING id::text",
    )
    .bind(&client_id)
    .bind(&title)
    .bind(b["body"].as_str().unwrap_or(""))
    .bind(severity)
    .bind(b["persistent"].as_bool().unwrap_or(false))
    .bind(b["expires_hours"].as_i64().unwrap_or(0))
    .bind(user.map(|u| u.email).unwrap_or_else(|| "admin".into()))
    .fetch_one(&s.db)
    .await;
    match res {
        Ok(row) => {
            use sqlx::Row;
            let id: String = row.get(0);
            audit(&s.db, "note", "create", &id).await;
            crate::notes::push_all(&s.sessions, &s.db).await;
            Json(json!({"ok":true,"id":id})).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "note create failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn delete_note(
    State(s): State<AdminState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let res = sqlx::query("DELETE FROM client_notes WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => {
            audit(&s.db, "note", "delete", &id).await;
            crate::notes::push_all(&s.sessions, &s.db).await;
            Json(json!({"ok":true})).into_response()
        }
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ---------------- deployments / virtual models ----------------

async fn list_deployments(State(s): State<AdminState>) -> Json<Value> {
    type DepRow = (
        uuid::Uuid,
        uuid::Uuid,
        String,
        String,
        String,
        bool,
        String,
        Value,
        Option<i64>,
        Option<i64>,
    );
    let rows: Vec<DepRow> = sqlx::query_as(
        "SELECT d.id, d.client_id, d.name, d.backend_type, d.model_name, d.enabled,
                    c.name AS client_name, d.capabilities,
                    d.kv_budget_tokens, d.reported_kv_budget_tokens
             FROM deployments d LEFT JOIN clients c ON c.id = d.client_id ORDER BY d.name",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    // VM members + router dep: refs per deployment — lets the dashboard show
    // what breaks before a deployment gets disabled or deleted.
    let member_refs: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.deployment_id::text, vm.api_name FROM virtual_model_members m
         JOIN virtual_models vm ON vm.id = m.virtual_model_id",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    let mut ref_map: std::collections::HashMap<String, Vec<String>> = Default::default();
    for (dep, vm) in member_refs {
        ref_map.entry(dep).or_default().push(vm);
    }
    let router_cfgs: Vec<(String, Value)> =
        sqlx::query_as("SELECT api_name, router_config FROM virtual_models WHERE kind='router'")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    let mut out = Vec::new();
    for (id, cid, n, bt, mn, en, cn, caps, kvb, rkvb) in rows {
        let dep_id = id.to_string();
        let rt = s.registry.dep_runtime(&dep_id).await.ok().flatten();
        let mut referenced_by = ref_map.remove(&dep_id).unwrap_or_default();
        let dep_ref = format!("dep:{dep_id}");
        let direct = crate::router_engine::dep_vm_name(&dep_id);
        for (rname, cfg) in &router_cfgs {
            let hit = cfg["members"].as_array().into_iter().flatten().any(|m| {
                m["ref"].as_str() == Some(dep_ref.as_str())
                    || m["resolved"].as_str() == Some(direct.as_str())
            });
            if hit {
                referenced_by.push(rname.clone());
            }
        }
        out.push(json!({"id":id,"client_id":cid,"client_name":cn,"name":n,"backend_type":bt,"model_name":mn,"enabled":en,
            "capabilities": caps, "referenced_by": referenced_by,
            "kv_budget_tokens": kvb, "reported_kv_budget_tokens": rkvb,
            "kv_budget_effective": kvb.or(rkvb),
            "runtime": rt.map(|r| json!({"state":r.state,"tier2_free":r.tier2_free,"tier2_total":r.tier2_total,"pressure_gated":r.pressure_gated,"kv_used":r.kv_used}))}));
    }
    Json(json!({"deployments": out}))
}

/// Router api_names whose config references `dep_id` (`dep:` member ref or a
/// materialized `resolved` name) — used for delete warnings/cleanup.
async fn router_refs_to_dep(s: &AdminState, dep_id: &str) -> Vec<String> {
    let rows: Vec<(String, Value)> =
        sqlx::query_as("SELECT api_name, router_config FROM virtual_models WHERE kind='router'")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    let dep_ref = format!("dep:{dep_id}");
    let direct = crate::router_engine::dep_vm_name(dep_id);
    rows.into_iter()
        .filter(|(_, cfg)| {
            cfg["members"].as_array().into_iter().flatten().any(|m| {
                m["ref"].as_str() == Some(dep_ref.as_str())
                    || m["resolved"].as_str() == Some(direct.as_str())
            })
        })
        .map(|(n, _)| n)
        .collect()
}

/// Router api_names whose config references a `vm:<api_name>` member.
async fn router_refs_to_vm(s: &AdminState, api_name: &str) -> Vec<String> {
    let rows: Vec<(String, Value)> =
        sqlx::query_as("SELECT api_name, router_config FROM virtual_models WHERE kind='router'")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    let vm_ref = format!("vm:{api_name}");
    rows.into_iter()
        .filter(|(_, cfg)| {
            cfg["members"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m["ref"].as_str() == Some(vm_ref.as_str()))
        })
        .map(|(n, _)| n)
        .collect()
}

/// VMs + routers that reference `dep_id` — reported by the deployment
/// delete/disable endpoints so a stale ref never silently breaks routing.
async fn dep_referenced_by(s: &AdminState, dep_id: &str) -> Vec<String> {
    let mut names: Vec<String> = sqlx::query_scalar(
        "SELECT vm.api_name FROM virtual_model_members m
         JOIN virtual_models vm ON vm.id = m.virtual_model_id
         WHERE m.deployment_id = $1::uuid",
    )
    .bind(dep_id)
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    names.extend(router_refs_to_dep(s, dep_id).await);
    names
}

/// Delete `kind='direct'` models no router member materializes anymore —
/// they exist only as dispatch targets for `dep:` refs and accumulate
/// otherwise. Direct models kept alive via `fallback_virtual_model_id`
/// stay. Returns how many were removed.
async fn gc_direct_vms(s: &AdminState) -> u64 {
    let cfgs: Vec<Value> =
        sqlx::query_scalar("SELECT router_config FROM virtual_models WHERE kind='router'")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    let mut keep: std::collections::HashSet<String> = Default::default();
    for cfg in &cfgs {
        for m in cfg["members"].as_array().into_iter().flatten() {
            if let Some(r) = m["resolved"].as_str() {
                keep.insert(r.to_string());
            }
            if let Some(dep) = m["ref"].as_str().and_then(|r| r.strip_prefix("dep:")) {
                keep.insert(crate::router_engine::dep_vm_name(dep));
            }
        }
    }
    let fallback_ids: Vec<String> = sqlx::query_scalar(
        "SELECT fallback_virtual_model_id::text FROM virtual_models
         WHERE fallback_virtual_model_id IS NOT NULL",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    let directs: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, api_name FROM virtual_models WHERE kind='direct'")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
    let mut removed = 0u64;
    for (id, name) in directs {
        if keep.contains(&name) || fallback_ids.iter().any(|f| f == &id.to_string()) {
            continue;
        }
        let r = sqlx::query("DELETE FROM virtual_models WHERE id=$1 AND kind='direct'")
            .bind(id)
            .execute(&s.db)
            .await;
        removed += r.map(|x| x.rows_affected()).unwrap_or(0);
    }
    removed
}

/// Soft-delete (`enabled=false`) or hard-delete (`?hard=true`) a deployment.
/// Soft keeps history/FKs and takes effect immediately in the routing
/// snapshot; hard cascades member rows and reloads the registry. Either way
/// the response lists every VM/router still referencing it.
async fn delete_deployment(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let hard = q
        .get("hard")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let exists: Option<(String,)> =
        sqlx::query_as("SELECT model_name FROM deployments WHERE id=$1::uuid")
            .bind(&id)
            .fetch_optional(&s.db)
            .await
            .unwrap_or(None);
    let Some((model_name,)) = exists else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"no such deployment"})),
        )
            .into_response();
    };
    let referenced_by = dep_referenced_by(&s, &id).await;
    // Push the desired state to the owning client so the change sticks:
    // hard → `removed` drops it from the client's own config (the next
    // inventory report no longer carries it and the upsert can't
    // re-create the row); soft → `enabled:false` so reports keep it
    // disabled instead of flipping it back on. Desired state persists in
    // Postgres — an offline client still applies it on next GetConfig,
    // and apply_inventory skips removed ids as a safety net.
    let client_id: Option<String> =
        sqlx::query_scalar("SELECT client_id::text FROM deployments WHERE id=$1::uuid")
            .bind(&id)
            .fetch_optional(&s.db)
            .await
            .unwrap_or(None);
    if let Some(cid) = client_id {
        let path = format!("{{deployments,{id}}}");
        let desired_dep = if hard {
            json!({"removed": true})
        } else {
            json!({"enabled": false})
        };
        let row: Option<(i64, Value)> = sqlx::query_as(
            "UPDATE clients SET desired_config = jsonb_set(
                 jsonb_set(COALESCE(desired_config,'{}'::jsonb), '{deployments}',
                           COALESCE(desired_config->'deployments','{}'::jsonb), true),
                 $2::text[],
                 COALESCE(desired_config->'deployments'->$3,'{}'::jsonb) || $4::jsonb,
                 true),
                 config_version = config_version + 1
             WHERE id = $1::uuid RETURNING config_version, desired_config",
        )
        .bind(&cid)
        .bind(&path)
        .bind(&id)
        .bind(&desired_dep)
        .fetch_optional(&s.db)
        .await
        .unwrap_or(None);
        if let Some((version, desired)) = row {
            let payload = serde_json::to_vec(&desired).unwrap_or_else(|_| b"{}".to_vec());
            let _ = s
                .sessions
                .send(
                    &cid,
                    loom_protocol::server_msgs::config_push(version.max(0) as u64, payload, false),
                )
                .await;
        }
    }
    if hard {
        let _ = sqlx::query("DELETE FROM deployments WHERE id=$1::uuid")
            .bind(&id)
            .execute(&s.db)
            .await;
        // Runtime keys have no FK — clean them explicitly.
        let mut c = s.registry.rt.conn.clone();
        let _: () = redis::cmd("DEL")
            .arg(s.registry.rt.k(&format!("dep:{id}")))
            .arg(s.registry.rt.k(&format!("dep:{id}:leases")))
            .query_async(&mut c)
            .await
            .unwrap_or(());
        let gc = gc_direct_vms(&s).await;
        let _ = s.models.reload().await;
        audit(&s.db, "deployment", "delete", &id).await;
        return Json(json!({
            "ok": true, "deleted": true, "model_name": model_name,
            "referenced_by": referenced_by, "gc_direct_vms": gc,
        }))
        .into_response();
    }
    let _ = sqlx::query("UPDATE deployments SET enabled=false, updated_at=now() WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    s.models.patch_deployment(&id, None, Some(false)).await;
    let mut c = s.registry.rt.conn.clone();
    let _: () = redis::cmd("HSET")
        .arg(s.registry.rt.k(&format!("dep:{id}")))
        .arg("state")
        .arg("unavailable")
        .query_async(&mut c)
        .await
        .unwrap_or(());
    audit(&s.db, "deployment", "disable", &id).await;
    Json(json!({
        "ok": true, "disabled": true, "model_name": model_name,
        "referenced_by": referenced_by,
    }))
    .into_response()
}

/// Update deployment fields the dashboard owns. Currently the only
/// server-side knob is the KV token budget override — `kv_budget_tokens`
/// accepts a positive integer, or null/0 to clear the override and fall
/// back to the client-reported adapter budget (or slot-only admission).
async fn update_deployment(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let get = |k: &str| b.get(k).cloned().unwrap_or(Value::Null);
    // null / 0 / absent -> clear the override; >0 -> set it
    let budget: Option<i64> = match get("kv_budget_tokens").as_i64() {
        Some(v) if v > 0 => Some(v),
        _ => None,
    };
    let res = sqlx::query(
        "UPDATE deployments SET kv_budget_tokens = $2, updated_at = now() \
         WHERE id = $1::uuid",
    )
    .bind(&id)
    .bind(budget)
    .execute(&s.db)
    .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => {
            // The routing snapshot carries the effective budget — reload so
            // the next dispatch sees the new value immediately.
            let _ = s.models.reload().await;
            // Mirror the *effective* budget (override ?? adapter-reported)
            // into the owning client's desired state — its local admission
            // backstop must gate on the same number the server does.
            let row: Option<(String, Option<i64>)> = sqlx::query_as(
                "SELECT client_id::text, reported_kv_budget_tokens \
                 FROM deployments WHERE id = $1::uuid",
            )
            .bind(&id)
            .fetch_optional(&s.db)
            .await
            .unwrap_or(None);
            if let Some((cid, reported)) = row {
                let effective = budget.or(reported).unwrap_or(0);
                let path = format!("{{deployments,{id}}}");
                let pushed: Option<(i64, Value)> = sqlx::query_as(
                    "UPDATE clients SET desired_config = jsonb_set(
                         jsonb_set(COALESCE(desired_config,'{}'::jsonb), '{deployments}',
                                   COALESCE(desired_config->'deployments','{}'::jsonb), true),
                         $2::text[],
                         COALESCE(desired_config->'deployments'->$3,'{}'::jsonb) || $4::jsonb,
                         true),
                         config_version = config_version + 1
                     WHERE id = $1::uuid RETURNING config_version, desired_config",
                )
                .bind(&cid)
                .bind(&path)
                .bind(&id)
                .bind(json!({"kv_budget_tokens": effective}))
                .fetch_optional(&s.db)
                .await
                .unwrap_or(None);
                if let Some((version, desired)) = pushed {
                    let payload = serde_json::to_vec(&desired).unwrap_or_else(|_| b"{}".to_vec());
                    let _ = s
                        .sessions
                        .send(
                            &cid,
                            loom_protocol::server_msgs::config_push(
                                version.max(0) as u64,
                                payload,
                                false,
                            ),
                        )
                        .await;
                }
            }
            audit(&s.db, "deployment", "update", &id).await;
            Json(json!({"ok": true, "kv_budget_tokens": budget})).into_response()
        }
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn list_vms(State(s): State<AdminState>) -> Json<Value> {
    type VmListRow = (
        uuid::Uuid,
        String,
        String,
        String,
        String,
        bool,
        bool,
        String,
    );
    let rows: Vec<VmListRow> = sqlx::query_as(
        "SELECT id, api_name, api_type, routing_strategy, description, enabled, allow_heterogeneous, kind FROM virtual_models WHERE kind <> 'direct' ORDER BY api_name")
        .fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"models": rows.iter().map(|(id,n,t,st,d,e,h,k)| json!({"id":id,"api_name":n,"api_type":t,"routing_strategy":st,"description":d,"enabled":e,"allow_heterogeneous":h,"kind":k})).collect::<Vec<_>>()}),
    )
}

async fn create_vm(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let name = b["api_name"].as_str().unwrap_or("");
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"api_name required"})),
        )
            .into_response();
    }
    let mut tx = match s.db.begin().await {
        Ok(t) => t,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let res = sqlx::query(
        "INSERT INTO virtual_models (api_name,description,api_type,routing_strategy,required_capabilities,allow_heterogeneous,fallback_virtual_model_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7::uuid) RETURNING id::text")
        .bind(name)
        .bind(b["description"].as_str().unwrap_or(""))
        .bind(b["api_type"].as_str().unwrap_or("chat"))
        .bind(b["routing_strategy"].as_str().unwrap_or("priority"))
        .bind(b.get("required_capabilities").cloned().unwrap_or(json!({})))
        .bind(b["allow_heterogeneous"].as_bool().unwrap_or(false))
        .bind(b["fallback_virtual_model_id"].as_str())
        .fetch_one(&mut *tx).await;
    let Ok(row) = res else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"api_name exists or bad fallback"})),
        )
            .into_response();
    };
    use sqlx::Row;
    let id: String = row.get(0);
    // Members are accepted on create — silently dropping them produced VMs
    // that route nowhere (observed: 200 create, later 503 no-deployment).
    // FK violations roll the whole create back instead of half-applying.
    if let Some(members) = b.get("members").and_then(|v| v.as_array()) {
        for m in members {
            let dep = m["deployment_id"].as_str().unwrap_or("");
            if dep.is_empty() {
                continue;
            }
            let r = sqlx::query(
                "INSERT INTO virtual_model_members (virtual_model_id,deployment_id,priority,weight,manual_pinned,enabled)
                 VALUES ($1::uuid,$2::uuid,$3,$4,$5,$6)")
                .bind(&id)
                .bind(dep)
                .bind(m["priority"].as_i64().unwrap_or(100) as i32)
                .bind(m["weight"].as_i64().unwrap_or(1) as i32)
                .bind(m["manual_pinned"].as_bool().unwrap_or(false))
                .bind(m["enabled"].as_bool().unwrap_or(true))
                .execute(&mut *tx)
                .await;
            if r.is_err() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"invalid member deployment_id"})),
                )
                    .into_response();
            }
        }
    }
    if tx.commit().await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let _ = s.models.reload().await;
    bump_version(&s.db, "virtual_model", &id, &b).await;
    audit(&s.db, "virtual_model", "create", &id).await;
    Json(json!({"ok":true,"id":id})).into_response()
}

async fn vm_detail(State(s): State<AdminState>, Path(id): Path<String>) -> Json<Value> {
    type VmRow = (
        uuid::Uuid,
        String,
        String,
        String,
        String,
        bool,
        bool,
        Option<uuid::Uuid>,
        serde_json::Value,
    );
    let vm: Option<VmRow> = sqlx::query_as(
        "SELECT id, api_name, api_type, routing_strategy, description, enabled, allow_heterogeneous, fallback_virtual_model_id, required_capabilities FROM virtual_models WHERE id=$1::uuid")
        .bind(&id).fetch_optional(&s.db).await.unwrap_or(None);
    let members: Vec<(uuid::Uuid, uuid::Uuid, i32, i32, bool, bool)> = sqlx::query_as(
        "SELECT id, deployment_id, priority, weight, manual_pinned, enabled FROM virtual_model_members WHERE virtual_model_id=$1::uuid")
        .bind(&id).fetch_all(&s.db).await.unwrap_or_default();
    Json(json!({
        "model": vm.map(|(i,n,t,st,d,e,h,fb,rc)| json!({"id":i,"api_name":n,"api_type":t,"routing_strategy":st,"description":d,"enabled":e,"allow_heterogeneous":h,"fallback_virtual_model_id":fb,"required_capabilities":rc})),
        "members": members.iter().map(|(mid,d,p,w,mp,e)| json!({"id":mid,"deployment_id":d,"priority":p,"weight":w,"manual_pinned":mp,"enabled":e})).collect::<Vec<_>>(),
    }))
}

async fn update_vm(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let _ = sqlx::query(
        "UPDATE virtual_models SET description=COALESCE($2,description), api_type=COALESCE($3,api_type),
         routing_strategy=COALESCE($4,routing_strategy), required_capabilities=COALESCE($5,required_capabilities),
         allow_heterogeneous=COALESCE($6,allow_heterogeneous),
         fallback_virtual_model_id=CASE WHEN $9::bool THEN $7::uuid ELSE fallback_virtual_model_id END,
         enabled=COALESCE($8,enabled), updated_at=now() WHERE id=$1::uuid")
        .bind(&id)
        .bind(b.get("description").and_then(|v| v.as_str()))
        .bind(b.get("api_type").and_then(|v| v.as_str()))
        .bind(b.get("routing_strategy").and_then(|v| v.as_str()))
        .bind(b.get("required_capabilities").cloned())
        .bind(b.get("allow_heterogeneous").and_then(|v| v.as_bool()))
        .bind(b.get("fallback_virtual_model_id").and_then(|v| v.as_str()))
        .bind(b.get("enabled").and_then(|v| v.as_bool()))
        .bind(b.get("fallback_virtual_model_id").is_some())
        .execute(&s.db).await;
    if let Some(members) = b.get("members").and_then(|v| v.as_array()) {
        let mut tx = match s.db.begin().await {
            Ok(t) => t,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let _ = sqlx::query("DELETE FROM virtual_model_members WHERE virtual_model_id=$1::uuid")
            .bind(&id)
            .execute(&mut *tx)
            .await;
        for m in members {
            let dep = m["deployment_id"].as_str().unwrap_or("");
            if dep.is_empty() {
                continue;
            }
            let _ = sqlx::query(
                "INSERT INTO virtual_model_members (virtual_model_id,deployment_id,priority,weight,manual_pinned,enabled)
                 VALUES ($1::uuid,$2::uuid,$3,$4,$5,$6)")
                .bind(&id).bind(dep)
                .bind(m["priority"].as_i64().unwrap_or(100) as i32)
                .bind(m["weight"].as_i64().unwrap_or(1) as i32)
                .bind(m["manual_pinned"].as_bool().unwrap_or(false))
                .bind(m["enabled"].as_bool().unwrap_or(true))
                .execute(&mut *tx).await;
        }
        let _ = tx.commit().await;
    }
    let _ = s.models.reload().await;
    bump_version(&s.db, "virtual_model", &id, &b).await;
    audit(&s.db, "virtual_model", "update", &id).await;
    Json(json!({"ok":true})).into_response()
}

async fn delete_vm(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    // Routers referencing this VM via `vm:` members keep the stale ref —
    // filtered at route time, but the caller should know.
    let api_name: Option<String> =
        sqlx::query_scalar("SELECT api_name FROM virtual_models WHERE id=$1::uuid")
            .bind(&id)
            .fetch_optional(&s.db)
            .await
            .unwrap_or(None);
    let referenced_by = match &api_name {
        Some(n) => router_refs_to_vm(&s, n).await,
        None => Vec::new(),
    };
    let _ = sqlx::query("DELETE FROM virtual_models WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    let _ = s.models.reload().await;
    audit(&s.db, "virtual_model", "delete", &id).await;
    Json(json!({"ok":true, "referenced_by": referenced_by})).into_response()
}

// ---------------- scheduler / monitoring ----------------

async fn sched_state(State(s): State<AdminState>) -> Json<Value> {
    let mut c = s.scheduler.rt().conn.clone();
    let classes = 4u32;
    let mut queues = Vec::new();
    for p in 0..classes {
        let n: i64 = redis::cmd("ZCARD")
            .arg(s.scheduler.rt().k(&format!("queue:{p}")))
            .query_async(&mut c)
            .await
            .unwrap_or(0);
        queues.push(json!({"class": p, "depth": n}));
    }
    Json(json!({"queues": queues}))
}

async fn cancel_request(State(s): State<AdminState>, Path(id): Path<String>) -> Json<Value> {
    Json(json!({"cancelled": s.scheduler.cancel(&id).await}))
}

#[derive(Deserialize)]
struct HistQ {
    limit: Option<i64>,
    #[allow(dead_code)]
    status: Option<String>,
}

async fn history(State(s): State<AdminState>, Query(q): Query<HistQ>) -> Json<Value> {
    type HistRow = (
        uuid::Uuid,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        Option<String>,
        Option<String>,
    );
    let limit = q.limit.unwrap_or(100).min(500);
    let rows: Vec<HistRow> = sqlx::query_as(
        "SELECT id, status, queue_wait_ms, ttft_ms, duration_ms, prompt_tokens, completion_tokens, error_code, created_at, client_id::text AS client_id, deployment_id::text AS deployment_id FROM requests_history ORDER BY created_at DESC LIMIT $1")
        .bind(limit).fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"requests": rows.iter().map(|(id,st,qw,t,d,pt,ct,ec,ca,cid,did)| json!({"id":id,"status":st,"queue_wait_ms":qw,"ttft_ms":t,"duration_ms":d,"prompt_tokens":pt,"completion_tokens":ct,"error_code":ec,"created_at":ca,"client_id":cid,"deployment_id":did})).collect::<Vec<_>>()}),
    )
}

async fn usage_stats(State(s): State<AdminState>) -> Json<Value> {
    let rows: Vec<(uuid::Uuid, i64, i64, i64)> = sqlx::query_as(
        "SELECT project_id, sum(requests)::bigint, sum(prompt_tokens)::bigint, sum(completion_tokens)::bigint FROM usage_counters WHERE bucket > now() - interval '24 hours' GROUP BY project_id")
        .fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"usage_24h": rows.iter().map(|(p,r,pt,ct)| json!({"project_id":p,"requests":r,"prompt_tokens":pt,"completion_tokens":ct})).collect::<Vec<_>>()}),
    )
}

/// Map a window selector to (postgres interval, bucket width seconds).
/// Bucket widths target ~50 points per chart so axes stay readable.
fn stats_window(w: Option<&String>) -> (&'static str, i64) {
    match w.map(|s| s.as_str()).unwrap_or("24h") {
        "7d" => ("7 days", 10_800),
        "30d" => ("30 days", 43_200),
        "365d" | "1y" => ("365 days", 604_800),
        "all" => ("100 years", 2_592_000),
        _ => ("24 hours", 1_800),
    }
}

#[derive(Deserialize)]
struct StatsQ {
    window: Option<String>,
}

/// Time-bucketed aggregates for dashboard charts. Bucketing is done with
/// epoch arithmetic so arbitrary bucket widths work (not just date_trunc units).
async fn stats_series(State(s): State<AdminState>, Query(q): Query<StatsQ>) -> Json<Value> {
    let (interval, bucket) = stats_window(q.window.as_ref());
    type Row = (
        chrono::DateTime<chrono::Utc>,
        i64,
        i64,
        i64,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        i64,
        i64,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT to_timestamp(floor(extract(epoch from created_at)/$2)*$2) AS bucket,
                count(*)::bigint,
                sum(prompt_tokens)::bigint,
                sum(completion_tokens)::bigint,
                avg(NULLIF(ttft_ms,0))::float8,
                percentile_cont(0.5)  within group (order by NULLIF(duration_ms,0))::float8,
                percentile_cont(0.95) within group (order by NULLIF(duration_ms,0))::float8,
                percentile_cont(0.99) within group (order by NULLIF(duration_ms,0))::float8,
                sum(duration_ms)::bigint,
                sum(GREATEST(duration_ms - ttft_ms, 0))::bigint
           FROM requests_history
          WHERE created_at > now() - $1::interval
          GROUP BY bucket ORDER BY bucket",
    )
    .bind(interval)
    .bind(bucket as f64)
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    Json(json!({
        "window": q.window.unwrap_or_else(|| "24h".into()),
        "bucket_secs": bucket,
        "buckets": rows.iter().map(|(t,r,pt,ct,ttft,p50,p95,p99,dur,dec)| json!({
            "t": t, "requests": r, "prompt_tokens": pt, "completion_tokens": ct,
            "ttft_ms": ttft.unwrap_or(0.0), "p50_ms": p50.unwrap_or(0.0),
            "p95_ms": p95.unwrap_or(0.0), "p99_ms": p99.unwrap_or(0.0),
            "duration_ms": dur, "decode_ms": dec,
        })).collect::<Vec<_>>()
    }))
}

/// Totals grouped by dimension (model / client / api key / project user) for
/// "total tokens by X" displays. duration/decode sums are included so clients
/// can derive e2e and decode tok/s without a second aggregation pass.
async fn stats_breakdown(State(s): State<AdminState>, Query(q): Query<StatsQ>) -> Json<Value> {
    let (interval, _) = stats_window(q.window.as_ref());
    type Row = (
        Option<String>,
        Option<uuid::Uuid>,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
    );
    // (resolved display name, project name for keys, raw id, requests, pt, ct, dur, dec)
    let run = |group: &str| {
        match group {
            "model" => "SELECT coalesce(vm.api_name, h.virtual_model_id::text, 'unknown'), NULL::uuid, '',
                        count(*)::bigint, sum(h.prompt_tokens)::bigint, sum(h.completion_tokens)::bigint,
                        sum(h.duration_ms)::bigint, sum(GREATEST(h.duration_ms - h.ttft_ms,0))::bigint
                   FROM requests_history h LEFT JOIN virtual_models vm ON vm.id = h.virtual_model_id
                  WHERE h.created_at > now() - $1::interval
                  GROUP BY vm.api_name, h.virtual_model_id ORDER BY sum(h.prompt_tokens)+sum(h.completion_tokens) DESC",
            "client" => "SELECT coalesce(c.name, h.client_id::text, 'unknown'), NULL::uuid, '',
                        count(*)::bigint, sum(h.prompt_tokens)::bigint, sum(h.completion_tokens)::bigint,
                        sum(h.duration_ms)::bigint, sum(GREATEST(h.duration_ms - h.ttft_ms,0))::bigint
                   FROM requests_history h LEFT JOIN clients c ON c.id = h.client_id
                  WHERE h.created_at > now() - $1::interval
                  GROUP BY c.name, h.client_id ORDER BY sum(h.prompt_tokens)+sum(h.completion_tokens) DESC",
            "key" => "SELECT coalesce(nullif(k.name,''), k.prefix, h.api_key_id::text, 'unknown'), NULL::uuid,
                        coalesce(p.name,''),
                        count(*)::bigint, sum(h.prompt_tokens)::bigint, sum(h.completion_tokens)::bigint,
                        sum(h.duration_ms)::bigint, sum(GREATEST(h.duration_ms - h.ttft_ms,0))::bigint
                   FROM requests_history h
                   LEFT JOIN api_keys k ON k.id = h.api_key_id
                   LEFT JOIN projects p ON p.id = h.project_id
                  WHERE h.created_at > now() - $1::interval
                  GROUP BY k.name, k.prefix, p.name, h.api_key_id ORDER BY sum(h.prompt_tokens)+sum(h.completion_tokens) DESC",
            _ => "SELECT coalesce(p.name, h.project_id::text, 'unknown'), NULL::uuid, '',
                        count(*)::bigint, sum(h.prompt_tokens)::bigint, sum(h.completion_tokens)::bigint,
                        sum(h.duration_ms)::bigint, sum(GREATEST(h.duration_ms - h.ttft_ms,0))::bigint
                   FROM requests_history h LEFT JOIN projects p ON p.id = h.project_id
                  WHERE h.created_at > now() - $1::interval
                  GROUP BY p.name, h.project_id ORDER BY sum(h.prompt_tokens)+sum(h.completion_tokens) DESC",
        }
    };
    let db = s.db.clone();
    let fetch = move |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, Row>(sql)
                .bind(interval)
                .fetch_all(&db)
                .await
                .unwrap_or_default()
        }
    };
    let map_rows = |rows: Vec<Row>| -> Vec<Value> {
        rows.into_iter()
            .map(|(name, _u, extra, r, pt, ct, dur, dec)| {
                json!({
                    "name": name.unwrap_or_else(|| "unknown".into()),
                    "extra": if extra.is_empty() { Value::Null } else { json!(extra) },
                    "requests": r, "prompt_tokens": pt, "completion_tokens": ct,
                    "tokens": pt + ct, "duration_ms": dur, "decode_ms": dec,
                })
            })
            .collect()
    };
    let models = map_rows(fetch(run("model")).await);
    let clients = map_rows(fetch(run("client")).await);
    let keys = map_rows(fetch(run("key")).await);
    let projects = map_rows(fetch(run("project")).await);
    Json(json!({
        "window": q.window.unwrap_or_else(|| "24h".into()),
        "models": models, "clients": clients, "keys": keys, "projects": projects,
    }))
}

/// Delete requests_history rows whose model/client/deployment reference no
/// longer exists (entities have no FKs, so hard-deletes leave orphan stats).
/// Also sweeps Redis runtime keys for deleted deployments/clients.
async fn purge_orphan_stats(State(s): State<AdminState>) -> Json<Value> {
    let dep_rows: i64 = sqlx::query_scalar(
        "WITH d AS (DELETE FROM requests_history WHERE deployment_id IS NOT NULL
                     AND deployment_id NOT IN (SELECT id FROM deployments) RETURNING 1)
         SELECT count(*) FROM d",
    )
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);
    let client_rows: i64 = sqlx::query_scalar(
        "WITH d AS (DELETE FROM requests_history WHERE client_id IS NOT NULL
                     AND client_id NOT IN (SELECT id FROM clients) RETURNING 1)
         SELECT count(*) FROM d",
    )
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);
    let model_rows: i64 = sqlx::query_scalar(
        "WITH d AS (DELETE FROM requests_history WHERE virtual_model_id IS NOT NULL
                     AND virtual_model_id NOT IN (SELECT id FROM virtual_models) RETURNING 1)
         SELECT count(*) FROM d",
    )
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);
    // Redis runtime keys for entities that no longer have a DB row.
    let mut c = s.registry.rt.conn.clone();
    let live_deps: Vec<String> = sqlx::query_scalar("SELECT id::text FROM deployments")
        .fetch_all(&s.db)
        .await
        .unwrap_or_default();
    let live_clients: Vec<String> = sqlx::query_scalar("SELECT id::text FROM clients")
        .fetch_all(&s.db)
        .await
        .unwrap_or_default();
    let prefix = s.registry.rt.k("");
    let mut cursor: u64 = 0;
    let mut redis_keys = 0u64;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{prefix}*"))
            .arg("COUNT")
            .arg(500)
            .query_async(&mut c)
            .await
            .unwrap_or_default();
        for k in keys {
            let short = &k[prefix.len()..];
            let id = short
                .trim_start_matches("dep:")
                .trim_start_matches("client:")
                .trim_end_matches(":leases");
            let orphan = if let Some(kind) = short.split(':').next() {
                match kind {
                    "dep" => !live_deps.iter().any(|d| d == id),
                    "client" => !live_clients.iter().any(|d| d == id),
                    _ => false,
                }
            } else {
                false
            };
            if orphan {
                let _: () = redis::cmd("DEL")
                    .arg(&k)
                    .query_async(&mut c)
                    .await
                    .unwrap_or(());
                redis_keys += 1;
            }
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    audit(&s.db, "stats", "purge_orphans", "requests_history").await;
    Json(json!({
        "ok": true,
        "deleted_history_rows": {
            "deployment": dep_rows,
            "client": client_rows,
            "model": model_rows,
        },
        "deleted_redis_keys": redis_keys,
    }))
}

// ---------------- settings / branding / system ----------------

async fn get_setting(State(s): State<AdminState>, Path(key): Path<String>) -> Json<Value> {
    match s.settings.get(&key).await {
        Ok(Some(v)) => Json(json!({"key": key, "value": v})),
        _ => Json(json!({"key": key, "value": null})),
    }
}

async fn put_setting(
    State(s): State<AdminState>,
    Path(key): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if ![
        "scheduler",
        "queue",
        "branding",
        "retention",
        "public_base",
        "release_repo",
    ]
    .contains(&key.as_str())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown setting"})),
        )
            .into_response();
    }
    let _ = s.settings.set(&key, b["value"].clone()).await;
    bump_version(&s.db, "settings", &key, &b["value"]).await;
    audit(&s.db, "settings", "put", &key).await;
    Json(json!({"ok":true})).into_response()
}

async fn branding(State(s): State<AdminState>) -> Json<Value> {
    Json(s.settings.branding().await.unwrap_or(json!({})))
}

async fn put_branding(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let mut cur = s.settings.branding().await.unwrap_or(json!({}));
    for k in ["app_name", "tagline", "accent_color"] {
        if let Some(v) = b.get(k).and_then(|v| v.as_str()) {
            if k == "accent_color" && !v.starts_with('#') {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"accent_color must be #rrggbb"})),
                )
                    .into_response();
            }
            cur[k] = json!(v);
        }
    }
    let _ = s.settings.set(crate::settings::KEY_BRANDING, cur).await;
    s.events.publish("branding", &json!({}));
    audit(&s.db, "branding", "put", "").await;
    Json(json!({"ok":true})).into_response()
}

async fn brand_logo(State(s): State<AdminState>, Path(ext): Path<String>) -> Response {
    match s.settings.logo_path().await {
        Ok(Some(p)) if std::path::Path::new(&p).exists() => match tokio::fs::read(&p).await {
            Ok(bytes) => Response::builder()
                .header(
                    "content-type",
                    match ext.as_str() {
                        "png" => "image/png",
                        "webp" => "image/webp",
                        _ => "image/jpeg",
                    },
                )
                .body(Body::from(bytes))
                .unwrap(),
            _ => StatusCode::NOT_FOUND.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn upload_logo(State(s): State<AdminState>, mut mp: Multipart) -> Response {
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.file_name().unwrap_or("logo.png").to_string();
        let ext = name.rsplit('.').next().unwrap_or("png").to_lowercase();
        if let Ok(bytes) = field.bytes().await {
            match s.settings.save_logo(&ext, &bytes).await {
                Ok(_) => {
                    s.events.publish("branding", &json!({}));
                    audit(&s.db, "branding", "logo", "").await;
                    return Json(json!({"ok":true})).into_response();
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response()
                }
            }
        }
    }
    (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response()
}

async fn config_versions(State(s): State<AdminState>) -> Json<Value> {
    type CfgVerRow = (
        i64,
        String,
        String,
        serde_json::Value,
        String,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<CfgVerRow> = sqlx::query_as(
        "SELECT id, entity, entity_id, change, actor, created_at FROM config_versions ORDER BY id DESC LIMIT 100")
        .fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"versions": rows.iter().map(|(i,e,eid,c,a,t)| json!({"id":i,"entity":e,"entity_id":eid,"change":c,"actor":a,"created_at":t})).collect::<Vec<_>>()}),
    )
}

async fn system_info(State(s): State<AdminState>) -> Json<Value> {
    let db_ok = sqlx::query("SELECT 1").execute(&s.db).await.is_ok();
    let mut c = s.scheduler.rt().conn.clone();
    let redis_ok: bool = redis::cmd("PING")
        .query_async::<String>(&mut c)
        .await
        .map(|v| v == "PONG")
        .unwrap_or(false);
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "postgres": db_ok,
        "redis": redis_ok,
    }))
}

async fn trigger_backup(State(s): State<AdminState>) -> Response {
    // Runs pg_dump directly (no shell): the data_dir is never interpolated into
    // a shell string and pg_dump gets the DB URL as a plain argv element.
    let db_url = if s.settings.database_url.is_empty() {
        std::env::var("LOOM_DATABASE_URL").unwrap_or_default()
    } else {
        s.settings.database_url.clone()
    };
    if db_url.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"database url not configured"})),
        )
            .into_response();
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("backup-{ts}.sql");
    let dir = std::path::Path::new(&s.settings.data_dir);
    let _ = tokio::fs::create_dir_all(dir).await;
    let path = dir.join(&name);
    // Plain std::fs::File (not tokio::fs::File::into_std): tokio keeps the fd
    // O_NONBLOCK and a child writing to it fails with EAGAIN → empty dump.
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("create backup file: {e}")})),
            )
                .into_response()
        }
    };
    // pg_dump discovery: explicit override → well-known install dirs (keg-only
    // libpq on macOS, postgresql client packages on Linux) → PATH fallback.
    let bin = std::env::var("LOOM_PG_DUMP").ok().unwrap_or_else(|| {
        [
            "/opt/homebrew/opt/libpq/bin/pg_dump",
            "/usr/local/opt/libpq/bin/pg_dump",
            "/usr/lib/postgresql/17/bin/pg_dump",
            "/usr/lib/postgresql/16/bin/pg_dump",
        ]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "pg_dump".to_string())
    });
    // .status(), not .output(): output() forcibly pipes stdout/stderr and
    // would discard the dump instead of writing it to the file.
    let out = tokio::process::Command::new(&bin)
        .arg(&db_url)
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .stderr(std::process::Stdio::inherit())
        .status()
        .await;
    match out {
        Ok(o) if o.success() => Json(json!({"ok":true, "file": name})).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = tokio::fs::remove_file(&path).await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"pg_dump not found — install postgresql client tools or set LOOM_PG_DUMP"})),
            )
                .into_response()
        }
        _ => {
            let _ = tokio::fs::remove_file(&path).await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"backup failed"})),
            )
                .into_response()
        }
    }
}

// ---------------- helpers ----------------

async fn audit(db: &PgPool, entity: &str, action: &str, entity_id: &str) {
    let _ = sqlx::query(
        "INSERT INTO audit_logs (actor, action, entity, entity_id) VALUES ('admin',$1,$2,$3)",
    )
    .bind(action)
    .bind(entity)
    .bind(entity_id)
    .execute(db)
    .await;
}

async fn bump_version(db: &PgPool, entity: &str, entity_id: &str, change: &Value) {
    let _ = sqlx::query(
        "INSERT INTO config_versions (entity, entity_id, change, actor) VALUES ($1,$2,$3,'admin')",
    )
    .bind(entity)
    .bind(entity_id)
    .bind(change)
    .execute(db)
    .await;
}

/// Check GitHub for a newer release vs CARGO_PKG_VERSION.
async fn update_check() -> Json<Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .user_agent("loom-server")
        .build()
        .unwrap_or_default();
    let latest = match client
        .get("https://api.github.com/repos/maxk2222/loom/releases/latest")
        .send()
        .await
    {
        Ok(r) => r.json::<Value>().await.ok(),
        Err(_) => None,
    };
    Json(json!({
        "current": env!("CARGO_PKG_VERSION"),
        "latest": latest.as_ref().and_then(|v| v["tag_name"].as_str().map(String::from)),
        "url": latest.as_ref().and_then(|v| v["html_url"].as_str().map(String::from)),
    }))
}

// ---------------- auto-routers ----------------

/// Ensure a hidden `kind='direct'` virtual model exists wrapping a single
/// deployment, so router members can point at deployments directly.
async fn ensure_direct_vm(s: &AdminState, dep_id: &str) -> Result<String, StatusCode> {
    let api_name = crate::router_engine::dep_vm_name(dep_id);
    let exists: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT id FROM virtual_models WHERE api_name = $1")
            .bind(&api_name)
            .fetch_optional(&s.db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if exists.is_some() {
        return Ok(api_name);
    }
    let dep: Option<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, model_name FROM deployments WHERE id = $1::uuid")
            .bind(dep_id)
            .fetch_optional(&s.db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some((dep_uuid, model_name)) = dep else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let vm_id: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO virtual_models (api_name, description, api_type, kind, allow_heterogeneous)
         VALUES ($1, $2, 'chat', 'direct', true) RETURNING id",
    )
    .bind(&api_name)
    .bind(format!("direct deployment {model_name}"))
    .fetch_one(&s.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = sqlx::query(
        "INSERT INTO virtual_model_members (virtual_model_id, deployment_id, priority)
         VALUES ($1, $2, 100) ON CONFLICT DO NOTHING",
    )
    .bind(vm_id.0)
    .bind(dep_uuid)
    .execute(&s.db)
    .await;
    Ok(api_name)
}

fn router_row_to_json(
    id: uuid::Uuid,
    api_name: String,
    description: String,
    enabled: bool,
    cfg: Value,
) -> Value {
    json!({"id": id, "api_name": api_name, "description": description,
           "enabled": enabled, "config": cfg})
}

async fn list_routers(State(s): State<AdminState>) -> Json<Value> {
    let rows: Vec<(uuid::Uuid, String, String, bool, Value)> = sqlx::query_as(
        "SELECT id, api_name, description, enabled, router_config
         FROM virtual_models WHERE kind = 'router' ORDER BY api_name",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    Json(
        json!({"routers": rows.into_iter().map(|r| router_row_to_json(r.0,r.1,r.2,r.3,r.4)).collect::<Vec<_>>()}),
    )
}

async fn create_router(State(s): State<AdminState>, Json(b): Json<Value>) -> Response {
    let name = b["api_name"].as_str().unwrap_or("");
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"api_name required"})),
        )
            .into_response();
    }
    let mut cfg = b["config"].clone();
    if !cfg.is_object() {
        cfg = json!({});
    }
    if let Err(code) = validate_router_cfg(&s, &cfg).await {
        return (code, Json(json!({"error":"invalid router config: engine/fallback must be real models, members required"}))).into_response();
    }
    if let Err(code) = materialize_members(&s, &mut cfg).await {
        return (code, Json(json!({"error":"bad member ref"}))).into_response();
    }
    let res = sqlx::query(
        "INSERT INTO virtual_models (api_name, description, api_type, routing_strategy, kind, router_config, enabled)
         VALUES ($1,$2,'chat','priority','router',$3,$4) RETURNING id::text",
    )
    .bind(name)
    .bind(b["description"].as_str().unwrap_or(""))
    .bind(&cfg)
    .bind(b["enabled"].as_bool().unwrap_or(true))
    .fetch_one(&s.db)
    .await;
    match res {
        Ok(row) => {
            use sqlx::Row;
            let id: String = row.get(0);
            let gc = gc_direct_vms(&s).await;
            let _ = s.models.reload().await;
            audit(&s.db, "router", "create", &id).await;
            Json(json!({"ok":true,"id":id,"gc_direct_vms":gc})).into_response()
        }
        Err(_) => (
            StatusCode::CONFLICT,
            Json(json!({"error":"api_name exists"})),
        )
            .into_response(),
    }
}

async fn update_router(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let mut cfg = b["config"].clone();
    if cfg.is_object() {
        if let Err(code) = validate_router_cfg(&s, &cfg).await {
            return (code, Json(json!({"error":"invalid router config"}))).into_response();
        }
        if let Err(code) = materialize_members(&s, &mut cfg).await {
            return (code, Json(json!({"error":"bad member ref"}))).into_response();
        }
    }
    let sets = json!({
        "api_name": b["api_name"].as_str(),
        "description": b["description"].as_str(),
        "router_config": cfg,
        "enabled": b["enabled"].as_bool(),
    });
    let res = sqlx::query(
        "UPDATE virtual_models SET
            api_name = COALESCE($2, api_name),
            description = COALESCE($3, description),
            router_config = CASE WHEN $4::jsonb IS NULL THEN router_config ELSE $4 END,
            enabled = COALESCE($5, enabled),
            updated_at = now()
         WHERE id = $1::uuid AND kind = 'router'",
    )
    .bind(&id)
    .bind(sets["api_name"].as_str())
    .bind(sets["description"].as_str())
    .bind(if cfg.is_object() {
        Some(cfg.clone())
    } else {
        None
    })
    .bind(b["enabled"].as_bool())
    .execute(&s.db)
    .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => {
            let gc = gc_direct_vms(&s).await;
            let _ = s.models.reload().await;
            audit(&s.db, "router", "update", &id).await;
            Json(json!({"ok":true,"gc_direct_vms":gc})).into_response()
        }
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"no such router"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"db"})),
        )
            .into_response(),
    }
}

async fn delete_router(State(s): State<AdminState>, Path(id): Path<String>) -> Response {
    let r = sqlx::query("DELETE FROM virtual_models WHERE id=$1::uuid AND kind='router'")
        .bind(&id)
        .execute(&s.db)
        .await;
    match r {
        Ok(x) if x.rows_affected() > 0 => {
            let gc = gc_direct_vms(&s).await;
            let _ = s.models.reload().await;
            audit(&s.db, "router", "delete", &id).await;
            Json(json!({"ok":true,"gc_direct_vms":gc})).into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"no such router"})),
        )
            .into_response(),
    }
}

/// Router config sanity: engine/fallback must resolve to non-router models,
/// at least one member required. `mode: "heuristic"` needs no engine model
/// but requires a catch-all member (empty/absent `when`) or a fallback.
async fn validate_router_cfg(s: &AdminState, cfg: &Value) -> Result<(), StatusCode> {
    let heuristic = cfg["mode"].as_str() == Some("heuristic");
    if !heuristic
        && cfg["router_model"]
            .as_str()
            .map(|v| v.is_empty())
            .unwrap_or(true)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    for k in ["router_model", "fallback_model"] {
        if let Some(n) = cfg[k].as_str().filter(|v| !v.is_empty()) {
            match s.models.resolve(n).await {
                Some(v) if v.kind != "router" => {}
                _ => return Err(StatusCode::BAD_REQUEST),
            }
        }
    }
    if cfg["members"]
        .as_array()
        .map(|m| m.is_empty())
        .unwrap_or(true)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if heuristic {
        let has_fallback = cfg["fallback_model"]
            .as_str()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let catch_all = cfg["members"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|m| m["when"].as_object().map(|o| o.is_empty()).unwrap_or(true));
        if !has_fallback && !catch_all {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    // `vm:` member refs must resolve to a real non-router model — otherwise
    // the router would either 503 at dispatch time or recurse. `dep:` refs
    // are checked by materialize_members (unknown ids → 400).
    for m in cfg["members"].as_array().into_iter().flatten() {
        if let Some(name) = m["ref"].as_str().and_then(|r| r.strip_prefix("vm:")) {
            match s.models.resolve(name).await {
                Some(v) if v.kind != "router" => {}
                _ => return Err(StatusCode::BAD_REQUEST),
            }
        }
        if !when_keys_valid(&m["when"]) || !member_extras_valid(m) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    Ok(())
}

/// `when` rule keys — unknown keys would silently never match, so typos are
/// rejected at write time. Recurses into `any`/`not` sub-rules.
fn when_keys_valid(when: &Value) -> bool {
    const KEYS: &[&str] = &[
        "chars_lt",
        "chars_gte",
        "tokens_lt",
        "tokens_gte",
        "msgs_lt",
        "msgs_gte",
        "code",
        "images",
        "audio",
        "system",
        "tools",
        "json_mode",
        "reasoning",
        "nonascii",
        "effort_gte",
        "effort_lt",
        "any",
        "not",
    ];
    let Some(o) = when.as_object() else {
        return when.is_null();
    };
    o.iter().all(|(k, v)| {
        KEYS.contains(&k.as_str())
            && match k.as_str() {
                "any" => v.as_array().is_some_and(|a| a.iter().all(when_keys_valid)),
                "not" => when_keys_valid(v),
                _ => true,
            }
    })
}

/// Member extras: `set` only allows generation-parameter keys (body merge
/// must not let a member hijack model/messages/stream); `tags` must be a
/// string array.
fn member_extras_valid(m: &Value) -> bool {
    const SET_KEYS: &[&str] = &[
        "reasoning_effort",
        "temperature",
        "top_p",
        "max_tokens",
        "presence_penalty",
        "frequency_penalty",
        "seed",
        "stop",
        "top_k",
        "repetition_penalty",
    ];
    const EFFORT_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    if let Some(set) = m["set"].as_object() {
        if set.keys().any(|k| !SET_KEYS.contains(&k.as_str())) {
            return false;
        }
        // reasoning_effort must be a known level — a typo would otherwise
        // reach the backend verbatim and mean something undefined.
        if let Some(e) = set.get("reasoning_effort") {
            match e.as_str() {
                Some(v) if EFFORT_LEVELS.contains(&v) => {}
                _ => return false,
            }
        }
    } else if !m["set"].is_null() {
        return false;
    }
    match &m["tags"] {
        Value::Null => true,
        Value::Array(a) => a.iter().all(|t| t.is_string()),
        _ => false,
    }
}

/// Materialize `dep:` member refs into hidden direct VMs. Returns Err(400)
/// when a ref points at nothing.
async fn materialize_members(s: &AdminState, cfg: &mut Value) -> Result<(), StatusCode> {
    let Some(ms) = cfg["members"].as_array_mut() else {
        return Ok(());
    };
    for m in ms.iter_mut() {
        let Some(refv) = m["ref"].as_str().map(String::from) else {
            continue;
        };
        if let Some(dep) = refv.strip_prefix("dep:") {
            let name = ensure_direct_vm(s, dep).await?;
            // The dispatch target is the materialized direct model's api_name.
            m["resolved"] = json!(name);
        } else if let Some(o) = m.as_object_mut() {
            // ref switched away from dep: — drop the stale materialization or
            // to_choice would keep routing to the old deployment.
            o.remove("resolved");
        }
    }
    Ok(())
}

/// Dry-run the router on a prompt — returns raw classifier output + pick.
async fn test_router(
    State(s): State<AdminState>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Json<Value> {
    let vm = match s.models.resolve_by_id(&id).await {
        Some(v) => Some(v),
        None => s.models.resolve(&id).await, // also allow api_name
    };
    let Some(vm) = vm else {
        return Json(json!({"ok": false, "error": "no such router"}));
    };
    let prompt = b["prompt"].as_str().unwrap_or("");
    Json(crate::router_engine::test_classify(&s.scheduler, &s.models, &vm, prompt, 10_000).await)
}

// ---------------- releases / updater ----------------

/// Resolve the GitHub release repo from settings (default: upstream LOOM).
async fn release_repo(s: &AdminState) -> String {
    s.settings
        .get("release_repo")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(String::from))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "maxk2222/loom".into())
}

/// Public manifest endpoint polled by clients (and the server self-update):
/// resolves the latest GitHub release for kind/channel/arch into the manifest
/// contract `loom_client::updater` expects. Asset naming:
/// `loom-{kind}-{version}-{arch}` + `.sha256` + `.minisig` sidecars.
/// 404 when no matching signed artifact exists.
async fn release_manifest(
    State(s): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let kind = q.get("kind").cloned().unwrap_or_else(|| "client".into());
    let channel = q.get("channel").cloned().unwrap_or_else(|| "stable".into());
    let arch = q.get("arch").cloned().unwrap_or_default();
    if !matches!(kind.as_str(), "client" | "server") || arch.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let cache_key = format!("{kind}|{channel}|{arch}");
    if let Some((at, v)) = s.release_cache.lock().unwrap().get(&cache_key) {
        if at.elapsed() < std::time::Duration::from_secs(600) {
            return Ok(Json(v.clone()));
        }
    }
    let repo = release_repo(&s).await;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("loom-server")
        .build()
        .unwrap_or_default();
    let release = fetch_release(&http, &repo, &channel)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let version = release["tag_name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('v')
        .to_string();
    let base_name = format!("loom-{kind}-{version}-{arch}");
    let assets = release["assets"].as_array().cloned().unwrap_or_default();
    let find = |suffix: &str| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            if name == format!("{base_name}{suffix}") {
                a["browser_download_url"].as_str().map(String::from)
            } else {
                None
            }
        })
    };
    let (Some(bin), Some(sum), Some(sig)) = (find(""), find(".sha256"), find(".minisig")) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let sha = fetch_text(&http, &sum).await.unwrap_or_default();
    let signature = fetch_text(&http, &sig).await.unwrap_or_default();
    if sha.is_empty() || signature.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let out = json!({
        "version": version,
        "channel": channel,
        "url": bin,
        "sha256": sha.split_whitespace().next().unwrap_or(""),
        "signature": signature,
        "min_version_protocol": loom_protocol::limits::PROTOCOL_VERSION_MIN,
    });
    s.release_cache
        .lock()
        .unwrap()
        .insert(cache_key, (std::time::Instant::now(), out.clone()));
    Ok(Json(out))
}

async fn fetch_release(http: &reqwest::Client, repo: &str, channel: &str) -> Option<Value> {
    if channel == "stable" {
        return http
            .get(format!(
                "https://api.github.com/repos/{repo}/releases/latest"
            ))
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
            .filter(|v| v["tag_name"].is_string());
    }
    let list = http
        .get(format!("https://api.github.com/repos/{repo}/releases"))
        .send()
        .await
        .ok()?
        .json::<Vec<Value>>()
        .await
        .ok()?;
    list.into_iter().find(|r| {
        let tag = r["tag_name"].as_str().unwrap_or_default();
        tag.contains(&format!("-{channel}")) || tag.starts_with(&format!("{channel}-"))
    })
}

async fn fetch_text(http: &reqwest::Client, url: &str) -> Option<String> {
    http.get(url)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Roll out a client update: bump config_version, merge an `update_now`
/// directive into desired_config and push it live over the control stream
/// (clients also pick it up via GetConfig on their next session).
async fn push_client_update(
    State(s): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    // Ship the manifest base along with the trigger — clients on custom gRPC
    // ports can't derive the HTTP endpoint from server_url (:50051→:8080
    // only covers defaults), so the server tells them explicitly.
    let base = public_base(&s).await;
    let updater = json!({"update_now": true, "server_base": base});
    // jsonb_set only creates the *last* missing path element — ensure the
    // intermediate `client` object exists first, then merge `updater` into it
    // (keeps other desired client fields intact).
    let row: Option<(i64, Value)> = sqlx::query_as(
        "UPDATE clients SET desired_config = jsonb_set(
             jsonb_set(COALESCE(desired_config,'{}'::jsonb), '{client}', COALESCE(desired_config->'client','{}'::jsonb), true),
             '{client,updater}', COALESCE(desired_config->'client'->'updater','{}'::jsonb) || $2::jsonb, true),
             config_version = config_version + 1
         WHERE id = $1::uuid RETURNING config_version, desired_config",
    )
    .bind(&id)
    .bind(&updater)
    .fetch_optional(&s.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some((version, desired)) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    let payload = serde_json::to_vec(&desired).unwrap_or_else(|_| b"{}".to_vec());
    let live = s
        .sessions
        .send(
            &id,
            loom_protocol::server_msgs::config_push(version.max(0) as u64, payload, false),
        )
        .await
        .is_ok();
    let _ = sqlx::query(
        "INSERT INTO config_versions (entity, entity_id, change, actor) VALUES ($1,$2,$3,'admin')",
    )
    .bind("client")
    .bind(&id)
    .bind(json!({"update_now": true}))
    .execute(&s.db)
    .await;
    Ok(Json(json!({ "ok": true, "live": live })))
}

/// Server self-update: download + verify + stage the `loom-server` release for
/// this platform, then exit — the supervisor (systemd/launchd/docker restart
/// policy) brings the new binary up. Cannot update while running on Windows.
async fn server_update(State(s): State<AdminState>) -> Json<Value> {
    #[cfg(windows)]
    return Json(json!({ "ok": false, "error": "self-update unsupported on windows" }));
    #[cfg(not(windows))]
    {
        let base = public_base(&s).await;
        let channel = "stable";
        let arch = server_arch();
        let url = format!("{base}/api/releases/manifest?kind=server&channel={channel}&arch={arch}");
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("loom-server")
            .build()
            .unwrap_or_default();
        let mf: Value = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return Json(json!({ "ok": false, "error": "no release for this platform" })),
        };
        let Some(bin_url) = mf["url"].as_str().map(String::from) else {
            return Json(json!({ "ok": false, "error": "manifest missing url" }));
        };
        let key = std::env::var("LOOM_UPDATE_PUBKEY").unwrap_or_default();
        if key.is_empty() {
            return Json(
                json!({ "ok": false, "error": "set LOOM_UPDATE_PUBKEY to the release signing key" }),
            );
        }
        let bytes = match http
            .get(&bin_url)
            .timeout(std::time::Duration::from_secs(300))
            .send()
            .await
        {
            Ok(r) => r.bytes().await.unwrap_or_default(),
            Err(_) => return Json(json!({ "ok": false, "error": "download failed" })),
        };
        if bytes.is_empty() {
            return Json(json!({ "ok": false, "error": "download failed" }));
        }
        let want = mf["sha256"].as_str().unwrap_or_default();
        let got = hex::encode(sha2::Sha256::digest(&bytes));
        if !got.eq_ignore_ascii_case(want) {
            return Json(json!({ "ok": false, "error": "sha256 mismatch" }));
        }
        let sig_str = mf["signature"].as_str().unwrap_or_default();
        let ok_sig = (|| -> anyhow::Result<()> {
            let pubkey = minisign_verify::PublicKey::from_base64(&key)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let sig =
                minisign_verify::Signature::decode(sig_str).map_err(|e| anyhow::anyhow!("{e}"))?;
            pubkey
                .verify(&bytes, &sig, false)
                .map_err(|e| anyhow::anyhow!("{e}"))
        })();
        if let Err(e) = ok_sig {
            return Json(json!({ "ok": false, "error": format!("signature: {e}") }));
        }
        // Swap: current_exe → .prev, staged → current_exe, then exit(0).
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return Json(json!({ "ok": false, "error": e.to_string() })),
        };
        let staged = exe.with_extension("new");
        let prev = exe.with_extension("prev");
        if let Err(e) = std::fs::write(&staged, &bytes) {
            return Json(json!({ "ok": false, "error": e.to_string() }));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
        }
        if let Err(e) = std::fs::rename(&exe, &prev) {
            return Json(json!({ "ok": false, "error": e.to_string() }));
        }
        if let Err(e) = std::fs::rename(&staged, &exe) {
            let _ = std::fs::rename(&prev, &exe);
            return Json(json!({ "ok": false, "error": e.to_string() }));
        }
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            std::process::exit(0);
        });
        Json(json!({ "ok": true, "restarting": true }))
    }
}

fn server_arch() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        _ => "unknown",
    }
}

/// Own HTTP base for self-requests (settings `public_base` or bind-derived).
async fn public_base(s: &AdminState) -> String {
    s.settings
        .get("public_base")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(String::from))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8080".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_cookie_only_over_https() {
        let plain = axum::http::HeaderMap::new();
        assert!(!cookie_is_secure(&plain, true));
        let mut fwd = axum::http::HeaderMap::new();
        fwd.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(cookie_is_secure(&fwd, true));
        // Untrusted mode ignores a spoofed proto header.
        assert!(!cookie_is_secure(&fwd, false));
        let mut http = axum::http::HeaderMap::new();
        http.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!cookie_is_secure(&http, true));
    }

    fn req_with_ip(peer: &str) -> Request {
        let mut r = Request::builder().body(Body::empty()).unwrap();
        r.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        r
    }

    #[test]
    fn client_ip_untrusted_uses_peer_only() {
        let mut r = req_with_ip("10.0.0.1:9000");
        r.headers_mut()
            .insert("cf-connecting-ip", "9.9.9.9".parse().unwrap());
        r.headers_mut()
            .insert("x-forwarded-for", "8.8.8.8, 7.7.7.7".parse().unwrap());
        // Spoofed headers must not leak into the bucket key.
        assert_eq!(client_ip(&r, false), Some("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn client_ip_trusted_prefers_cf_header() {
        let mut r = req_with_ip("127.0.0.1:9000");
        r.headers_mut()
            .insert("x-forwarded-for", "8.8.8.8".parse().unwrap());
        r.headers_mut()
            .insert("cf-connecting-ip", "9.9.9.9".parse().unwrap());
        assert_eq!(client_ip(&r, true), Some("9.9.9.9".parse().unwrap()));
        // XFF fallback when CF header absent; first hop wins.
        let mut r2 = req_with_ip("127.0.0.1:9000");
        r2.headers_mut()
            .insert("x-forwarded-for", "8.8.8.8, 7.7.7.7".parse().unwrap());
        assert_eq!(client_ip(&r2, true), Some("8.8.8.8".parse().unwrap()));
        // Garbage header values fall back to the peer, never parsed.
        let mut r3 = req_with_ip("127.0.0.1:9000");
        r3.headers_mut()
            .insert("cf-connecting-ip", "not-an-ip".parse().unwrap());
        assert_eq!(client_ip(&r3, true), Some("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn session_cookie_carries_flags() {
        assert!(session_cookie("t", true).contains("; Secure"));
        assert!(!session_cookie("t", false).contains("; Secure"));
    }

    #[tokio::test]
    async fn child_stdout_redirects_to_file() {
        let path = std::env::temp_dir().join("loom-redirect-test.txt");
        let std_file = std::fs::File::create(&path).unwrap();
        let out = tokio::process::Command::new("echo")
            .arg("loom-backup-test")
            .stdout(std_file)
            .status()
            .await
            .unwrap();
        assert!(out.success());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("loom-backup-test"), "got: {content:?}");
        let _ = std::fs::remove_file(&path);
    }
}
