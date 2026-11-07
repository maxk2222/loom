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
    extract::{Multipart, Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse},
    routing::{get, post, put},
    Json, Router,
};
use loom_core::ids::{opaque_token, sha256_hex};
use serde::Deserialize;
use serde_json::{json, Value};
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
}

pub fn router(s: AdminState) -> Router {
    let public = Router::new()
        .route("/api/branding", get(branding))
        .route("/api/update-check", get(update_check))
        .route("/api/branding/logo.{ext}", get(brand_logo))
        .route("/api/setup/status", get(setup_status))
        .route("/api/setup/admin", post(setup_admin))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
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
        .route("/api/keys/{id}", put(update_key))
        .route("/api/keys/{id}/revoke", post(revoke_key))
        // clients
        .route("/api/clients", get(list_clients))
        .route("/api/clients/{id}", get(client_detail))
        .route("/api/clients/{id}/status", post(client_status))
        .route("/api/enroll-tokens", post(issue_enroll_token))
        // deployments
        .route("/api/deployments", get(list_deployments))
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
/// over HTTPS (per proxy header) or the operator opted in via env — set
/// LOOM_SECURE_COOKIES=1 when serving the admin API behind TLS.
fn cookie_is_secure(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
        || std::env::var("LOOM_SECURE_COOKIES")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
}

async fn login(
    State(s): State<AdminState>,
    headers: axum::http::HeaderMap,
    Json(b): Json<LoginBody>,
) -> Response {
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let secure = cookie_is_secure(&headers);
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
    let (queued, active): (i64, i64) = (
        sqlx::query_scalar("SELECT count(*) FROM requests_history WHERE created_at > now() - interval '5 min' AND status = 'queued'").fetch_one(&s.db).await.unwrap_or(0),
        sqlx::query_scalar("SELECT count(*) FROM requests_history WHERE created_at > now() - interval '5 min' AND status = 'streaming'").fetch_one(&s.db).await.unwrap_or(0),
    );
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
        "queued_5m": queued,
        "active_5m": active,
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
    audit(&s.db, "client", "status", &format!("{id}:{status}")).await;
    Json(json!({"ok":true})).into_response()
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

// ---------------- deployments / virtual models ----------------

async fn list_deployments(State(s): State<AdminState>) -> Json<Value> {
    let rows: Vec<(uuid::Uuid, uuid::Uuid, String, String, String, bool)> = sqlx::query_as(
        "SELECT id, client_id, name, backend_type, model_name, enabled FROM deployments ORDER BY name")
        .fetch_all(&s.db).await.unwrap_or_default();
    let mut out = Vec::new();
    for (id, cid, n, bt, mn, en) in rows {
        let rt = s.registry.dep_runtime(&id.to_string()).await.ok().flatten();
        out.push(json!({"id":id,"client_id":cid,"name":n,"backend_type":bt,"model_name":mn,"enabled":en,
            "runtime": rt.map(|r| json!({"state":r.state,"tier2_free":r.tier2_free,"tier2_total":r.tier2_total}))}));
    }
    Json(json!({"deployments": out}))
}

async fn list_vms(State(s): State<AdminState>) -> Json<Value> {
    let rows: Vec<(uuid::Uuid, String, String, String, String, bool, bool)> = sqlx::query_as(
        "SELECT id, api_name, api_type, routing_strategy, description, enabled, allow_heterogeneous FROM virtual_models ORDER BY api_name")
        .fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"models": rows.iter().map(|(id,n,t,st,d,e,h)| json!({"id":id,"api_name":n,"api_type":t,"routing_strategy":st,"description":d,"enabled":e,"allow_heterogeneous":h})).collect::<Vec<_>>()}),
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
        .fetch_one(&s.db).await;
    match res {
        Ok(row) => {
            use sqlx::Row;
            let id: String = row.get(0);
            let _ = s.models.reload().await;
            bump_version(&s.db, "virtual_model", &id, &b).await;
            audit(&s.db, "virtual_model", "create", &id).await;
            Json(json!({"ok":true,"id":id})).into_response()
        }
        Err(_) => (
            StatusCode::CONFLICT,
            Json(json!({"error":"api_name exists or bad fallback"})),
        )
            .into_response(),
    }
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
         allow_heterogeneous=COALESCE($6,allow_heterogeneous), fallback_virtual_model_id=$7::uuid,
         enabled=COALESCE($8,enabled), updated_at=now() WHERE id=$1::uuid")
        .bind(&id)
        .bind(b.get("description").and_then(|v| v.as_str()))
        .bind(b.get("api_type").and_then(|v| v.as_str()))
        .bind(b.get("routing_strategy").and_then(|v| v.as_str()))
        .bind(b.get("required_capabilities").cloned())
        .bind(b.get("allow_heterogeneous").and_then(|v| v.as_bool()))
        .bind(b.get("fallback_virtual_model_id").and_then(|v| v.as_str()))
        .bind(b.get("enabled").and_then(|v| v.as_bool()))
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
    let _ = sqlx::query("DELETE FROM virtual_models WHERE id=$1::uuid")
        .bind(&id)
        .execute(&s.db)
        .await;
    let _ = s.models.reload().await;
    audit(&s.db, "virtual_model", "delete", &id).await;
    Json(json!({"ok":true})).into_response()
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
        "SELECT project_id, sum(requests), sum(prompt_tokens), sum(completion_tokens) FROM usage_counters WHERE bucket > now() - interval '24 hours' GROUP BY project_id")
        .fetch_all(&s.db).await.unwrap_or_default();
    Json(
        json!({"usage_24h": rows.iter().map(|(p,r,pt,ct)| json!({"project_id":p,"requests":r,"prompt_tokens":pt,"completion_tokens":ct})).collect::<Vec<_>>()}),
    )
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
    if !["scheduler", "queue", "branding", "retention"].contains(&key.as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_cookie_only_over_https() {
        let plain = axum::http::HeaderMap::new();
        assert!(!cookie_is_secure(&plain));
        let mut fwd = axum::http::HeaderMap::new();
        fwd.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(cookie_is_secure(&fwd));
        let mut http = axum::http::HeaderMap::new();
        http.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!cookie_is_secure(&http));
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
