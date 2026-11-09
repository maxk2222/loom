//! Local priority OpenAI API + local admin REST + embedded minimal UI.
//! Works fully without a server connection (local-first).

use crate::backend_manager::BackendManager;
use crate::config::ConfigStore;
use crate::connector::Connector;
use crate::scheduler::{LocalScheduler, RequestClass};
use crate::telemetry;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use loom_core::capability::{ApiType, BackendType, RequiredCaps};
use loom_core::config::BackendConfig;
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
    /// Local UI sessions: sha256(token) -> expiry (unix ms). In-memory only —
    /// a restart simply means logging in again.
    pub sessions: Arc<RwLock<HashMap<String, i64>>>,
    /// Admin notices pushed by the server (+ persistence path).
    pub notes: crate::notes::SharedNotes,
    pub notes_path: std::path::PathBuf,
    /// Live host metrics from the pressure monitor.
    pub sys_stats: crate::sysmon::SharedStats,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct FinReq {
    pub class: String,
    pub api: String,
    pub deployment_id: String,
    pub duration_ms: i64,
    pub ttft_ms: i64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub status: String,
    pub ended_unix_ms: i64,
}

/// Load a persisted history snapshot (best-effort; keeps the newest 300).
pub fn load_history_file(path: &std::path::Path) -> VecDeque<FinReq> {
    let Ok(bytes) = std::fs::read(path) else {
        return VecDeque::new();
    };
    let mut items: Vec<FinReq> = serde_json::from_slice(&bytes).unwrap_or_default();
    if items.len() > 300 {
        items.drain(..items.len() - 300);
    }
    items.into_iter().collect()
}

/// Persist the history ring (atomic via tmp+rename).
pub fn save_history_file(path: &std::path::Path, ring: &RwLock<VecDeque<FinReq>>) {
    let items: Vec<FinReq> = ring.read().unwrap().iter().cloned().collect();
    let Ok(json) = serde_json::to_vec(&items) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
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
            deployment_id: i.deployment_id,
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
        .route("/v1/images/generations", post(openai_images))
        .route("/v1/audio/speech", post(openai_speech))
        .route(
            "/v1/audio/transcriptions",
            post(openai_transcription)
                .route_layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route(
            "/v1/audio/translations",
            post(openai_translation)
                .route_layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route("/v1/videos", post(openai_videos))
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
        .route(
            "/local/api/connection",
            get(admin_connection).put(admin_put_connection),
        )
        .route("/local/api/metrics", get(admin_metrics))
        .route("/local/api/notes", get(admin_notes))
        .route("/local/api/notes/{id}/dismiss", post(admin_dismiss_note))
        .route("/local/api/update-check", get(admin_update_check))
        .route("/local/api/update", post(admin_update_apply))
        .route("/local/api/auth/status", get(auth_status))
        .route("/local/api/auth/setup", post(local_setup))
        .route("/local/api/auth/skip", post(local_skip))
        .route("/local/api/auth/login", post(local_login))
        .route("/local/api/auth/logout", post(local_logout))
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

/// Local API auth: passes when nothing is configured (open loopback, the
/// historical default), or with a valid Bearer api_key, or a session cookie
/// issued by the first-run setup wizard / login endpoint. Static UI assets,
/// liveness and the auth endpoints themselves stay reachable.
async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path();
    if path.starts_with("/local/api/auth/")
        || (req.method() == axum::http::Method::GET
            && (path == "/healthz" || path == "/" || path.starts_with("/ui")))
    {
        return Ok(next.run(req).await);
    }
    let cfg = state.cfg.get().await;
    let protected = !cfg.local_api.api_key.is_empty() || !cfg.local_api.user_hash.is_empty();
    if !protected {
        return Ok(next.run(req).await);
    }
    // Bearer api_key path.
    let key = &cfg.local_api.api_key;
    if !key.is_empty() {
        let ok = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|v| v == key.as_str())
            .unwrap_or(false);
        if ok {
            return Ok(next.run(req).await);
        }
    }
    // Session-cookie path (setup wizard / login).
    if let Some(tok) = req
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|p| p.trim().strip_prefix("loom_local_session="))
                .next()
        })
    {
        let th = loom_core::ids::sha256_hex(tok);
        let valid = state
            .sessions
            .read()
            .unwrap()
            .get(&th)
            .is_some_and(|exp| *exp > now_ms());
        if valid {
            return Ok(next.run(req).await);
        }
    }
    Err(StatusCode::UNAUTHORIZED)
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
async fn openai_images(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    openai_inference(s, ApiType::Images, headers, body).await
}
async fn openai_speech(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    openai_inference(s, ApiType::AudioSpeech, headers, body).await
}
async fn openai_videos(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    openai_inference(s, ApiType::Video, headers, body).await
}

/// Raw-body variants for multipart audio endpoints. `openai_inference`
/// takes an `InBody` so the downstream admit/forward/bridge flow is shared.
enum InBody {
    Json(Value),
    Multipart {
        bytes: bytes::Bytes,
        content_type: String,
    },
}

async fn openai_multipart(
    s: AppState,
    headers: HeaderMap,
    api: ApiType,
    body: bytes::Bytes,
) -> Response {
    let request_id = loom_core::ids::RequestId::new().to_string();
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let model = match multipart_model(&body, &content_type).await {
        Some(m) => m,
        None => {
            return err(
                ErrorCode::InvalidRequest,
                "missing model field",
                &request_id,
            )
        }
    };
    openai_inference_raw(
        s,
        api,
        headers,
        InBody::Multipart {
            bytes: body,
            content_type,
        },
        model,
        request_id,
    )
    .await
}

async fn openai_transcription(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    openai_multipart(s, headers, ApiType::AudioTranscription, body).await
}
async fn openai_translation(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    openai_multipart(s, headers, ApiType::AudioTranslation, body).await
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
    openai_inference_raw(s, api, headers, InBody::Json(body), model, request_id).await
}

/// Shared admit → forward → bridge flow for JSON and multipart bodies.
/// `model` is the caller-facing name (JSON field or multipart form field).
async fn openai_inference_raw(
    s: AppState,
    api: ApiType,
    headers: HeaderMap,
    ib: InBody,
    model: String,
    request_id: String,
) -> Response {
    let Some(dep_id) = s.backends.resolve_model(&model).await else {
        return err(ErrorCode::ModelNotFound, "no such model", &request_id);
    };

    // Capability check — only enforced when probing succeeded; unknown caps
    // defer to the backend (its own error surfaces upstream).
    let req_caps = match &ib {
        InBody::Json(b) => required_caps_from_request(b),
        InBody::Multipart { .. } => RequiredCaps::default(),
    };
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
            est_tokens: match &ib {
                InBody::Json(b) => loom_core::tokens::estimate_request_tokens(b),
                // rough proxy: ~1 token per 4 bytes of audio payload
                InBody::Multipart { bytes, .. } => (bytes.len() / 4) as u32,
            },
        },
    );

    let started = Instant::now();
    let cancel = CancellationToken::new();
    let resp = match &ib {
        InBody::Json(b) => {
            s.backends
                .forward(&dep_id, api, b.clone(), cancel.clone())
                .await
        }
        InBody::Multipart {
            bytes,
            content_type,
        } => {
            s.backends
                .forward_multipart(&dep_id, api, bytes, content_type, cancel.clone())
                .await
        }
    };

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
        "text/event-stream".to_string()
    } else if !resp.content_type.is_empty() {
        // upstream media type (audio/mpeg, application/json, …) — the
        // backend's own answer wins over a static guess.
        resp.content_type.clone()
    } else {
        "application/json".to_string()
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

/// Extract the `model` form field from a raw multipart request body — it
/// decides which local deployment serves the transcription/translation.
async fn multipart_model(body: &[u8], content_type: &str) -> Option<String> {
    let boundary = multer::parse_boundary(content_type).ok()?;
    let bytes = bytes::Bytes::copy_from_slice(body);
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(bytes) });
    let mut mp = multer::Multipart::new(stream, boundary);
    while let Ok(Some(field)) = mp.next_field().await {
        if field.name() == Some("model") {
            return field.text().await.ok().map(|s| s.trim().to_string());
        }
    }
    None
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
    let sys = s.sys_stats.read().unwrap().clone();
    Json(json!({
        "name": cfg.name,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime,
        "server_connected": s.connector.is_connected(),
        "server_url": cfg.server_url,
        "sharing_enabled": cfg.sharing_enabled,
        "capacities": caps,
        "system": sys,
        "update": crate::updater::watchdog_status(),
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
            if new_cfg.name != current.name
                || new_cfg.server_url != current.server_url
                || new_cfg.connect_enabled != current.connect_enabled
            {
                s.connector.notify_reconnect();
            }
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
    // Models the backend lists/loads that no deployment covers yet —
    // surfaced for the UI's one-click "add deployment" flow. Dynamic
    // deployments report their bound model alongside the "auto" config.
    let detected = s.backends.detected_models().await;
    let mut out = masked;
    for v in out.iter_mut() {
        let bid = v["id"].as_str().unwrap_or_default().to_string();
        if let Some(ds) = v["deployments"].as_array_mut() {
            for d in ds.iter_mut() {
                if let Some(did) = d["id"].as_str() {
                    let r = s.backends.effective_model(did).await;
                    d["resolved_model"] = json!(r);
                }
            }
        }
        v["detected_models"] = detected
            .get(&bid)
            .map(|ms| {
                json!(ms
                    .iter()
                    .map(|(m, l)| json!({"model": m, "loaded": l}))
                    .collect::<Vec<_>>())
            })
            .unwrap_or(json!([]));
    }
    Json(json!({"backends": out}))
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
    #[serde(default)]
    api_version: String,
}

async fn admin_test_backend(State(_s): State<AppState>, Json(b): Json<TestBody>) -> Json<Value> {
    if let Err(e) = validate_endpoint(&b.endpoint) {
        return Json(json!({"ok": false, "error": e}));
    }
    // Build a throwaway adapter so the test exercises the real URL scheme /
    // auth headers of the selected backend type.
    let bt: loom_core::capability::BackendType =
        serde_json::from_value(json!(b.backend_type)).unwrap_or(BackendType::GenericOpenAi);
    let cfg = BackendConfig {
        id: "__test".into(),
        name: "__test".into(),
        backend_type: bt,
        endpoint: b.endpoint.clone(),
        api_key: b.api_key.clone(),
        deployments: vec![],
        enabled: true,
        remote: true,
        api_version: if b.api_version.is_empty() {
            "2024-10-21".into()
        } else {
            b.api_version.clone()
        },
        health_interval_secs: 15,
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default();
    let adapter = crate::adapters::make_adapter(&cfg, client);
    let t = std::time::Instant::now();
    match adapter.list_models().await {
        Ok(models) => Json(json!({
            "ok": true,
            "latency_ms": t.elapsed().as_millis(),
            "models": models,
        })),
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
            let resolved = s.backends.effective_model(&d.id).await;
            out.push(json!({
                "deployment": d,
                "backend_id": b.id,
                "backend_type": b.backend_type,
                "capabilities": caps,
                "health": health.map(|h| h.as_str()),
                "resolved_model": resolved,
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

async fn admin_get_sharing(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.cfg.get().await;
    Json(json!({
        "sharing_enabled": cfg.sharing_enabled,
        "pressure": cfg.pressure,
    }))
}

async fn admin_put_sharing(State(s): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    // Accept both "sharing_enabled" and the legacy UI field "enabled".
    let sharing = body
        .get("sharing_enabled")
        .or_else(|| body.get("enabled"))
        .and_then(|v| v.as_bool());
    if let Some(v) = sharing {
        let _ = s.cfg.update(|c| c.sharing_enabled = v).await;
        s.scheduler.set_sharing_enabled(v);
        s.connector.notify_capacity();
        info!(sharing = v, "sharing toggled via local admin");
    }
    if let Some(p) = body.get("pressure").and_then(|v| v.as_object()) {
        let num = |k: &str| p.get(k).and_then(|v| v.as_u64());
        let _ = s
            .cfg
            .update(|c| {
                if let Some(v) = p.get("enabled").and_then(|v| v.as_bool()) {
                    c.pressure.enabled = v;
                }
                if let Some(v) = num("cpu_pct") {
                    c.pressure.cpu_pct = v.min(100) as u8;
                }
                if let Some(v) = num("ram_pct") {
                    c.pressure.ram_pct = v.min(100) as u8;
                }
                if let Some(v) = p.get("unload_models").and_then(|v| v.as_bool()) {
                    c.pressure.unload_models = v;
                }
                if let Some(v) = num("cooldown_secs") {
                    c.pressure.cooldown_secs = v;
                }
            })
            .await;
        info!("pressure limits updated via local admin");
    }
    let cfg = s.cfg.get().await;
    Json(json!({
        "sharing_enabled": cfg.sharing_enabled,
        "pressure": cfg.pressure,
    }))
}

async fn admin_reprobe(State(s): State<AppState>) -> Json<Value> {
    s.backends.reprobe().await;
    s.connector.notify_capacity();
    Json(json!({"ok": true}))
}

async fn admin_connection(State(s): State<AppState>) -> Json<Value> {
    let session = s.connector.state().session_id.read().unwrap().clone();
    let cfg = s.cfg.get().await;
    Json(json!({
        "connected": s.connector.is_connected(),
        "session_id": session,
        "name": cfg.name,
        "server_url": cfg.server_url,
        "client_id": cfg.client_id,
        "enabled": cfg.connect_enabled,
    }))
}

/// Update identity/connectivity fields shown in the dashboard. Changing the
/// name or toggling the connection drops the session so the next hello
/// carries the fresh values.
async fn admin_put_connection(State(s): State<AppState>, Json(b): Json<Value>) -> Response {
    let name = b.get("name").and_then(|v| v.as_str()).map(str::to_string);
    let url = b
        .get("server_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let enabled = b.get("enabled").and_then(|v| v.as_bool());
    if let Some(n) = &name {
        if n.trim().is_empty() || n.len() > 128 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"name must be 1..128 chars"})),
            )
                .into_response();
        }
    }
    let _ = s
        .cfg
        .update(|c| {
            if let Some(n) = name {
                c.name = n.trim().to_string();
            }
            if let Some(u) = url {
                c.server_url = u.trim().to_string();
            }
            if let Some(e) = enabled {
                c.connect_enabled = e;
            }
        })
        .await;
    s.connector.notify_reconnect();
    Json(json!({"ok":true})).into_response()
}

async fn admin_metrics(State(_s): State<AppState>) -> Json<Value> {
    Json(json!({"note": "prometheus metrics at the dedicated listener when configured"}))
}

/// Admin notices pushed by the server. Persistent briefings first, then
/// newest. `undismissed` counts what the bell badge shows.
async fn admin_notes(State(s): State<AppState>) -> Json<Value> {
    let notes = s.notes.read().unwrap().clone();
    let undismissed = notes.iter().filter(|n| !n.dismissed).count();
    let mut ordered = notes;
    ordered.sort_by(|a, b| {
        b.persistent
            .cmp(&a.persistent)
            .then(b.created_at_unix.cmp(&a.created_at_unix))
    });
    Json(json!({"notes": ordered, "undismissed": undismissed}))
}

/// Client-local dismissal. Persistent briefings cannot be dismissed.
async fn admin_dismiss_note(
    State(s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<Value> {
    let ok = crate::notes::dismiss(&s.notes, &id);
    if ok {
        crate::notes::save_notes(&s.notes_path, &s.notes);
    }
    Json(json!({"ok": ok}))
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

/// Manual update trigger: checks the server manifest and applies a pending
/// release (`apply_staged` + process exit; the supervisor restarts us).
/// `?apply=0` only reports whether an update is available.
async fn admin_update_apply(
    State(s): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let cfg = s.cfg.get().await;
    if q.get("apply").map(|v| v.as_str()) == Some("0") {
        let base = crate::updater::manifest_base(&cfg.updater, &cfg.server_url);
        if base.is_empty() {
            return Json(json!({ "ok": false, "error": "no server configured" }));
        }
        return match crate::updater::check_for_update(
            &base,
            &cfg.updater.channel,
            crate::updater::arch(),
            env!("CARGO_PKG_VERSION"),
        )
        .await
        {
            Ok((st, _)) => Json(serde_json::to_value(st).unwrap_or_default()),
            Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
        };
    }
    let dir = s
        .cfg
        .path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // On success run_once swaps the binary and exits — reply first.
    let cfg2 = cfg.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::updater::run_once(
            &cfg2.updater,
            &cfg2.server_url,
            env!("CARGO_PKG_VERSION"),
            &dir,
            true,
        )
        .await
        {
            warn!(error = %e, "manual update failed");
        }
    });
    Json(json!({ "ok": true, "applying": true }))
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

// ---------------- local UI auth (first-run wizard) ----------------

const LOCAL_SESSION_COOKIE: &str = "loom_local_session";
const SESSION_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

fn issue_session(s: &AppState) -> String {
    let token = loom_core::ids::opaque_token(32);
    s.sessions.write().unwrap().insert(
        loom_core::ids::sha256_hex(&token),
        now_ms() + SESSION_TTL_MS,
    );
    token
}

fn session_cookie(token: &str) -> String {
    format!("{LOCAL_SESSION_COOKIE}={token}; HttpOnly; Path=/; SameSite=Strict; Max-Age=604800")
}

/// Auth surface for the local UI: "open" (nothing configured), "setup"
/// (no user yet — show the first-run wizard), "login" (user exists).
async fn auth_status(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.cfg.get().await;
    let user_set = !cfg.local_api.user_hash.is_empty();
    let key_set = !cfg.local_api.api_key.is_empty();
    let mode = if user_set {
        "login"
    } else if !cfg.local_api.setup_done {
        "setup"
    } else if !key_set {
        "open"
    } else {
        "login"
    };
    Json(json!({"mode": mode, "user": cfg.local_api.user_name}))
}

/// First-run wizard: create the local UI user. Only usable while no user
/// exists — afterwards it is closed permanently.
async fn local_setup(State(s): State<AppState>, Json(b): Json<Value>) -> Response {
    let name = b["username"].as_str().unwrap_or("").trim().to_string();
    let pw = b["password"].as_str().unwrap_or("").to_string();
    if name.is_empty() || pw.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"username required, password ≥ 8 chars"})),
        )
            .into_response();
    }
    let already = !s.cfg.get().await.local_api.user_hash.is_empty();
    if already {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"user already configured"})),
        )
            .into_response();
    }
    let hash = match argon2::Argon2::default().hash_password(
        pw.as_bytes(),
        &argon2::password_hash::SaltString::generate(&mut argon2::password_hash::rand_core::OsRng),
    ) {
        Ok(h) => h.to_string(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let name2 = name.clone();
    if s.cfg
        .update(|c| {
            c.local_api.user_name = name;
            c.local_api.user_hash = hash;
            c.local_api.setup_done = true;
        })
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let token = issue_session(&s);
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::SET_COOKIE, session_cookie(&token))
        .body(Body::from(
            serde_json::to_string(&json!({"ok":true,"user":name2})).unwrap(),
        ))
        .unwrap()
}

/// Sign in: `{username,password}` against the stored user, or `{api_key}`
/// matching the configured bearer key — both mint the same session cookie.
async fn local_login(State(s): State<AppState>, Json(b): Json<Value>) -> Response {
    let cfg = s.cfg.get().await;
    let ok = if let Some(k) = b["api_key"].as_str() {
        !cfg.local_api.api_key.is_empty() && k == cfg.local_api.api_key
    } else {
        let name_ok = b["username"].as_str().unwrap_or("") == cfg.local_api.user_name;
        let pw = b["password"].as_str().unwrap_or("");
        name_ok
            && !cfg.local_api.user_hash.is_empty()
            && argon2::password_hash::PasswordHash::new(&cfg.local_api.user_hash)
                .map(|h| {
                    argon2::Argon2::default()
                        .verify_password(pw.as_bytes(), &h)
                        .is_ok()
                })
                .unwrap_or(false)
    };
    if !ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let token = issue_session(&s);
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::SET_COOKIE, session_cookie(&token))
        .body(Body::from(
            serde_json::to_string(&json!({"ok":true})).unwrap(),
        ))
        .unwrap()
}

async fn local_logout(State(s): State<AppState>, req: axum::extract::Request) -> Response {
    if let Some(tok) = req
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|p| p.trim().strip_prefix("loom_local_session="))
                .next()
        })
    {
        s.sessions
            .write()
            .unwrap()
            .remove(&loom_core::ids::sha256_hex(tok));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::SET_COOKIE,
            "loom_local_session=; HttpOnly; Path=/; SameSite=Strict; Max-Age=0",
        )
        .body(Body::empty())
        .unwrap()
}

/// Dismiss the first-run wizard permanently without creating a user —
/// the API stays open on loopback (or stays protected by api_key if set).
async fn local_skip(State(s): State<AppState>) -> Response {
    let _ = s.cfg.update(|c| c.local_api.setup_done = true).await;
    Json(json!({"ok":true})).into_response()
}
