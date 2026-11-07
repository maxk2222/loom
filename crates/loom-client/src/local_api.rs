//! Local priority OpenAI API + local admin REST + embedded minimal UI.
//! Works fully without a server connection (local-first).

use crate::backend_manager::BackendManager;
use crate::config::ConfigStore;
use crate::connector::Connector;
use crate::scheduler::{LocalScheduler, RequestClass};
use crate::telemetry;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use loom_core::capability::ApiType;
use loom_core::error::{ApiError, ErrorCode};
use loom_core::openai::{request_model, required_caps_from_request, ModelList};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<ConfigStore>,
    pub backends: BackendManager,
    pub scheduler: LocalScheduler,
    pub connector: Connector,
    pub started: Instant,
    /// Live request registry for the admin UI.
    pub requests: Arc<RwLock<HashMap<String, RequestInfo>>>,
    /// Finished-request ring buffer (charts).
    pub history: Arc<RwLock<VecDeque<FinReq>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinReq {
    pub class: String,
    pub api: String,
    pub duration_ms: i64,
    pub ttft_ms: i64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub status: String,
    pub ended_unix_ms: i64,
}

/// Remove an active request and append it to the history ring.
/// `pt`/`ct` are real backend-reported tokens, or the estimate stored at
/// admission time when the backend omitted usage.
pub fn finish_req(s: &AppState, rid: &str, status: &str, pt: u32, ct: u32, ttft_ms: i64) {
    if let Some(i) = s.requests.write().unwrap().remove(rid) {
        let mut h = s.history.write().unwrap();
        if h.len() >= 300 {
            h.pop_front();
        }
        h.push_back(FinReq {
            class: i.class,
            api: i.api,
            duration_ms: now_ms() - i.started_unix_ms,
            ttft_ms,
            prompt_tokens: if pt > 0 { pt } else { i.est_tokens },
            completion_tokens: ct,
            status: status.into(),
            ended_unix_ms: now_ms(),
        });
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestInfo {
    pub request_id: String,
    pub class: String,
    pub deployment_id: String,
    pub api: String,
    pub started_unix_ms: i64,
    /// Fallback token estimate when the backend omits `usage`.
    #[serde(skip)]
    pub est_tokens: u32,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        // OpenAI surface
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/completions", post(openai_completions))
        .route("/v1/embeddings", post(openai_embeddings))
        .route("/v1/models", get(openai_models))
        // local admin
        .route("/local/api/status", get(admin_status))
        .route(
            "/local/api/config",
            get(admin_get_config).put(admin_put_config),
        )
        .route(
            "/local/api/backends",
            get(admin_list_backends).post(admin_add_backend),
        )
        .route(
            "/local/api/backends/{id}",
            put(admin_update_backend).delete(admin_delete_backend),
        )
        .route("/local/api/backends/test", post(admin_test_backend))
        .route("/local/api/deployments", get(admin_list_deployments))
        .route("/local/api/requests", get(admin_list_requests))
        .route("/local/api/history", get(admin_history))
        .route(
            "/local/api/requests/{id}/cancel",
            post(admin_cancel_request),
        )
        .route(
            "/local/api/sharing",
            get(admin_get_sharing).put(admin_put_sharing),
        )
        .route("/local/api/reprobe", post(admin_reprobe))
        .route("/local/api/connection", get(admin_connection))
        .route("/local/api/metrics", get(admin_metrics))
        .route("/local/api/update-check", get(admin_update_check))
        .route("/healthz", get(|| async { "ok" }));

    let api = if state.cfg_blocking().local_api.serve_ui {
        api.merge(crate::local_ui::router()).route(
            "/",
            get(|| async { axum::response::Redirect::permanent("/ui/") }),
        )
    } else {
        api
    };

    api.layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ))
    .with_state(state)
}

impl AppState {
    fn cfg_blocking(&self) -> loom_core::config::ClientConfig {
        // ConfigStore is async; for the router-construction decision we read
        // synchronously once at startup via try_read fallback.
        futures::executor::block_on(self.cfg.get())
    }
}

/// Optional local API key check (only when configured).
async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let cfg = state.cfg.get().await;
    let key = &cfg.local_api.api_key;
    if key.is_empty() {
        return Ok(next.run(req).await);
    }
    // admin UI asset GETs pass through unauthenticated? No — protect everything
    // when a key is set; the UI itself prompts for the key.
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v == key.as_str())
        .unwrap_or(false);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ---------------------------------------------------------------------------
// OpenAI endpoints (priority class)
// ---------------------------------------------------------------------------

async fn openai_chat(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    openai_inference(s, ApiType::Chat, headers, body).await
}
async fn openai_completions(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    openai_inference(s, ApiType::Completions, headers, body).await
}
async fn openai_embeddings(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    openai_inference(s, ApiType::Embeddings, headers, body).await
}

async fn openai_models(State(s): State<AppState>) -> Json<Value> {
    let models = s.backends.local_models().await;
    Json(
        serde_json::to_value(ModelList::of(models, "loom-local"))
            .unwrap_or(json!({"object":"list","data":[]})),
    )
}

async fn openai_inference(s: AppState, api: ApiType, headers: HeaderMap, body: Value) -> Response {
    let request_id = loom_core::ids::RequestId::new().to_string();
    let model = match request_model(&body) {
        Some(m) => m.to_string(),
        None => return err(ErrorCode::InvalidRequest, "missing model", &request_id),
    };
    let Some(dep_id) = s.backends.resolve_model(&model).await else {
        return err(ErrorCode::ModelNotFound, "no such model", &request_id);
    };

    // Capability check.
    // Capability check — only enforced when probing succeeded; unknown caps
    // defer to the backend (its own error surfaces upstream).
    let req_caps = required_caps_from_request(&body);
    if let Some(caps) = s.backends.capabilities(&dep_id).await {
        if !caps.supports_api(api) {
            return err(
                ErrorCode::InvalidRequest,
                "model does not support this endpoint",
                &request_id,
            );
        }
        if !caps.satisfies(&req_caps) {
            return err(
                ErrorCode::InvalidRequest,
                "deployment lacks required capabilities (tools/vision/json_mode)",
                &request_id,
            );
        }
    }
    if let Some(h) = s.backends.health_of(&dep_id).await {
        if h == crate::adapters::Health::Down {
            return err(ErrorCode::Unavailable, "backend is down", &request_id);
        }
    }

    let token = match s
        .scheduler
        .try_admit(&dep_id, RequestClass::Local, &request_id)
    {
        Ok(t) => t,
        Err(r) => {
            telemetry::req_rejected("local", r.as_str());
            let code = match r {
                crate::scheduler::RejectReason::NoCapacity => ErrorCode::ConcurrencyExceeded,
                crate::scheduler::RejectReason::BackendDown => ErrorCode::Unavailable,
                _ => ErrorCode::Unavailable,
            };
            return err(code, r.as_str(), &request_id);
        }
    };
    telemetry::req_admitted("local");
    s.connector.notify_capacity();

    s.requests.write().unwrap().insert(
        request_id.clone(),
        RequestInfo {
            request_id: request_id.clone(),
            class: "local".into(),
            deployment_id: dep_id.clone(),
            api: api.as_str().into(),
            started_unix_ms: now_ms(),
            est_tokens: loom_core::tokens::estimate_request_tokens(&body),
        },
    );

    let started = Instant::now();
    let cancel = CancellationToken::new();
    let resp = s.backends.forward(&dep_id, api, body, cancel.clone()).await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            telemetry::req_finished("failed");
            s.scheduler.finish_request(&request_id);
            finish_req(&s, &request_id, "failed", 0, 0, 0);
            s.connector.notify_capacity();
            drop(token); // release the admission slot
            return err(ErrorCode::UpstreamError, e.to_string(), &request_id);
        }
    };

    // Bridge task: pumps upstream bytes -> response body. Holds the admission
    // token and upstream cancel for the whole stream lifetime; on client
    // disconnect (tx closed), error or stream end it cancels the upstream
    // request and releases bookkeeping.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
    {
        let sched = s.scheduler.clone();
        let conn = s.connector.clone();
        let rid = request_id.clone();
        let mut stream = resp.stream;
        tokio::spawn(async move {
            let _token = token; // keep slot until stream ends
            use futures_util::StreamExt;
            let mut usage = loom_core::openai::Usage::default();
            let mut finish_reason = String::new();
            let mut ttft_ms: i64 = 0;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => break,
                    v = stream.next() => v,
                };
                match next {
                    Some(Ok(b)) => {
                        if ttft_ms == 0 {
                            ttft_ms = started.elapsed().as_millis() as i64;
                        }
                        crate::connector::inspect_sse_tail(&b, &mut usage, &mut finish_reason);
                        if tx.send(Ok(b)).await.is_err() {
                            // caller disconnected -> abort upstream work
                            cancel.cancel();
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "upstream stream error");
                        let msg = bytes::Bytes::from(
                            "data: {\"error\":{\"message\":\"upstream stream error\"}}\n\n",
                        );
                        let _ = tx.send(Ok(msg)).await;
                        break;
                    }
                    None => break,
                }
            }
            telemetry::req_finished("done");
            telemetry::req_duration(started.elapsed().as_secs_f64(), "local");
            sched.finish_request(&rid);
            finish_req(
                &s,
                &rid,
                "done",
                usage.prompt_tokens,
                usage.completion_tokens,
                ttft_ms,
            );
            conn.notify_capacity();
        });
    }

    let content_type = if resp.is_event_stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let _ = headers;

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("x-request-id", &request_id)
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap_or_else(|_| err(ErrorCode::Internal, "response build", &request_id))
}

fn err(code: ErrorCode, msg: impl Into<String>, request_id: &str) -> Response {
    let e = ApiError::new(code, msg)
        .with_request_id(request_id)
        .envelope();
    (
        StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(e),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Local admin endpoints
// ---------------------------------------------------------------------------

async fn admin_status(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.cfg.get().await;
    let caps = s.scheduler.capacity_snapshot();
    let uptime = s.started.elapsed().as_secs();
    Json(json!({
        "name": cfg.name,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime,
        "server_connected": s.connector.is_connected(),
        "server_url": cfg.server_url,
        "sharing_enabled": cfg.sharing_enabled,
        "capacities": caps,
    }))
}

async fn admin_get_config(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.cfg.get().await;
    // Redact secrets.
    let mut v = serde_json::to_value(&cfg).unwrap_or(json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("device_credential".into(), json!("***"));
        if let Some(backends) = obj.get_mut("backends").and_then(|b| b.as_array_mut()) {
            for b in backends {
                if let Some(o) = b.as_object_mut() {
                    o.insert("api_key".into(), json!("***"));
                }
            }
        }
    }
    Json(v)
}

#[derive(Debug, Deserialize)]
struct PutConfigBody {
    config: loom_core::config::ClientConfig,
}

async fn admin_put_config(State(s): State<AppState>, Json(body): Json<PutConfigBody>) -> Response {
    let mut new_cfg = body.config;
    // Never let the UI overwrite the device credential accidentally.
    let current = s.cfg.get().await;
    if new_cfg.device_credential == "***" || new_cfg.device_credential.is_empty() {
        new_cfg.device_credential = current.device_credential.clone();
    }
    if new_cfg.client_id.is_empty() {
        new_cfg.client_id = current.client_id.clone();
    }
    for b in &mut new_cfg.backends {
        if b.api_key == "***" {
            if let Some(old) = current.backends.iter().find(|o| o.id == b.id) {
                b.api_key = old.api_key.clone();
            } else {
                b.api_key = String::new();
            }
        }
    }
    let res = s.cfg.update(|c| *c = new_cfg.clone()).await;
    match res {
        Ok(_) => {
            s.backends.sync_from_config(&new_cfg).await;
            let dep_cfgs: Vec<_> = new_cfg
                .backends
                .iter()
                .flat_map(|b| b.deployments.clone())
                .collect();
            s.scheduler.sync_deployments(&dep_cfgs);
            s.scheduler.set_sharing_enabled(new_cfg.sharing_enabled);
            s.connector.notify_capacity();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(ErrorCode::Internal, e.to_string(), "admin"),
    }
}

#[derive(Debug, Deserialize)]
struct BackendBody {
    backend: loom_core::config::BackendConfig,
}

async fn admin_list_backends(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.cfg.get().await;
    // Never echo secrets: mask api_key, expose only whether one is set.
    let masked: Vec<Value> = cfg
        .backends
        .iter()
        .map(|b| {
            let mut v = serde_json::to_value(b).unwrap_or_default();
            v["api_key"] = json!("");
            v["has_api_key"] = json!(!b.api_key.is_empty());
            v
        })
        .collect();
    Json(json!({"backends": masked}))
}

/// Endpoint must be a concrete http(s) URL — blocks file://, javascript:,
/// bare strings and (best-effort) keeps the operator from pointing the
/// adapter at non-HTTP schemes.
fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint.len() > 512 {
        return Err("endpoint too long".into());
    }
    let u = reqwest::Url::parse(endpoint.trim()).map_err(|e| e.to_string())?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err("endpoint must be http(s)".into());
    }
    if u.host_str().map(|h| h.is_empty()).unwrap_or(true) {
        return Err("endpoint has no host".into());
    }
    Ok(())
}

async fn admin_add_backend(State(s): State<AppState>, Json(body): Json<BackendBody>) -> Response {
    let mut b = body.backend;
    if let Err(e) = validate_endpoint(&b.endpoint) {
        return err(ErrorCode::InvalidRequest, e, "admin");
    }
    if b.id.is_empty() {
        b.id = uuid::Uuid::new_v4().to_string();
    }
    for d in &mut b.deployments {
        if d.id.is_empty() {
            d.id = uuid::Uuid::new_v4().to_string();
        }
        if d.name.len() > 128 || d.model_name.len() > 256 {
            return err(
                ErrorCode::InvalidRequest,
                "name/model_name too long",
                "admin",
            );
        }
    }
    // Preserve the stored api_key when the update sends none (the UI never
    // receives the plaintext key back, so it can't resend it).
    if b.api_key.is_empty() {
        let cfg = s.cfg.get().await;
        if let Some(old) = cfg.backends.iter().find(|x| x.id == b.id) {
            b.api_key = old.api_key.clone();
        }
    }
    let res = s
        .cfg
        .update(|c| {
            c.backends.retain(|x| x.id != b.id);
            c.backends.push(b.clone())
        })
        .await;
    match res {
        Ok(_) => {
            let _ = s.cfg.persist().await;
            let cfg = s.cfg.get().await;
            s.backends.sync_from_config(&cfg).await;
            let deps: Vec<_> = cfg
                .backends
                .iter()
                .flat_map(|b| b.deployments.clone())
                .collect();
            s.scheduler.sync_deployments(&deps);
            s.connector.notify_capacity();
            s.connector.state().push_inventory().await;
            Json(json!({"ok": true, "id": b.id})).into_response()
        }
        Err(e) => err(ErrorCode::Internal, e.to_string(), "admin"),
    }
}

async fn admin_update_backend(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<BackendBody>,
) -> Response {
    let mut b = body.backend;
    b.id = id.clone();
    admin_add_backend(State(s), Json(BackendBody { backend: b })).await
}

async fn admin_delete_backend(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let res = s.cfg.update(|c| c.backends.retain(|b| b.id != id)).await;
    match res {
        Ok(_) => {
            let _ = s.cfg.persist().await;
            let cfg = s.cfg.get().await;
            s.backends.sync_from_config(&cfg).await;
            let deps: Vec<_> = cfg
                .backends
                .iter()
                .flat_map(|b| b.deployments.clone())
                .collect();
            s.scheduler.sync_deployments(&deps);
            s.connector.notify_capacity();
            s.connector.state().push_inventory().await;
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(ErrorCode::Internal, e.to_string(), "admin"),
    }
}

/// Connectivity probe for the UI's "Test connection" button — hits the
/// endpoint's /v1/models without persisting anything.
#[derive(Debug, Deserialize)]
struct TestBody {
    endpoint: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    backend_type: String,
}

async fn admin_test_backend(State(_s): State<AppState>, Json(b): Json<TestBody>) -> Json<Value> {
    if let Err(e) = validate_endpoint(&b.endpoint) {
        return Json(json!({"ok": false, "error": e}));
    }
    let path = match b.backend_type.as_str() {
        "ollama" => "/api/tags",
        _ => "/v1/models",
    };
    let url = format!("{}{}", b.endpoint.trim_end_matches('/'), path);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default();
    let mut req = client.get(&url);
    if !b.api_key.is_empty() {
        req = req.bearer_auth(&b.api_key);
    }
    let t = std::time::Instant::now();
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let models: Vec<String> = if b.backend_type == "ollama" {
                body["models"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                body["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Json(json!({
                "ok": (200..300).contains(&status),
                "status": status,
                "latency_ms": t.elapsed().as_millis() as u64,
                "models": models,
            }))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn admin_list_deployments(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.cfg.get().await;
    let mut out = Vec::new();
    for b in &cfg.backends {
        for d in &b.deployments {
            let caps = s.backends.capabilities(&d.id).await;
            let health = s.backends.health_of(&d.id).await;
            out.push(json!({
                "deployment": d,
                "backend_id": b.id,
                "backend_type": b.backend_type,
                "capabilities": caps,
                "health": health.map(|h| h.as_str()),
            }));
        }
    }
    Json(json!({"deployments": out}))
}

async fn admin_list_requests(State(s): State<AppState>) -> Json<Value> {
    let reqs: Vec<RequestInfo> = s.requests.read().unwrap().values().cloned().collect();
    Json(json!({"requests": reqs}))
}

async fn admin_cancel_request(State(s): State<AppState>, Path(id): Path<String>) -> Json<Value> {
    let local = s.scheduler.cancel_request(&id);
    let tier2 = s.connector.abort_request(&id);
    Json(json!({"cancelled": local || tier2}))
}

#[derive(Debug, Serialize)]
struct SharingState {
    sharing_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct SharingBody {
    sharing_enabled: Option<bool>,
}

async fn admin_get_sharing(State(s): State<AppState>) -> Json<SharingState> {
    let cfg = s.cfg.get().await;
    Json(SharingState {
        sharing_enabled: cfg.sharing_enabled,
    })
}

async fn admin_put_sharing(
    State(s): State<AppState>,
    Json(body): Json<SharingBody>,
) -> Json<Value> {
    if let Some(v) = body.sharing_enabled {
        let _ = s.cfg.update(|c| c.sharing_enabled = v).await;
        s.scheduler.set_sharing_enabled(v);
        s.connector.notify_capacity();
        info!(sharing = v, "sharing toggled via local admin");
    }
    let cfg = s.cfg.get().await;
    Json(json!({"sharing_enabled": cfg.sharing_enabled}))
}

async fn admin_reprobe(State(s): State<AppState>) -> Json<Value> {
    s.backends.reprobe().await;
    s.connector.notify_capacity();
    Json(json!({"ok": true}))
}

async fn admin_connection(State(s): State<AppState>) -> Json<Value> {
    let session = s.connector.state().session_id.read().unwrap().clone();
    Json(json!({
        "connected": s.connector.is_connected(),
        "session_id": session,
    }))
}

async fn admin_metrics(State(_s): State<AppState>) -> Json<Value> {
    Json(json!({"note": "prometheus metrics at the dedicated listener when configured"}))
}

pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Check GitHub for a newer release vs CARGO_PKG_VERSION.
async fn admin_update_check() -> Json<Value> {
    let latest = fetch_latest_release().await;
    Json(json!({
        "current": env!("CARGO_PKG_VERSION"),
        "latest": latest.as_ref().map(|r| r.0.clone()),
        "url": latest.as_ref().map(|r| r.1.clone()),
    }))
}

pub(crate) async fn fetch_latest_release() -> Option<(String, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .user_agent("loom-client")
        .build()
        .ok()?;
    let v: Value = client
        .get("https://api.github.com/repos/maxk2222/loom/releases/latest")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some((
        v["tag_name"].as_str()?.to_string(),
        v["html_url"].as_str()?.to_string(),
    ))
}

/// Finished-request history for charts.
async fn admin_history(State(s): State<AppState>) -> Json<Value> {
    let h: Vec<FinReq> = s.history.read().unwrap().iter().cloned().collect();
    Json(json!({ "history": h }))
}
