//! OpenAI-compatible gateway: /v1/* endpoints, SSE bridging, error taxonomy.

use crate::auth::{ApiKeyCtx, Auth};
use crate::scheduler::{EndMeta, PendingRequest, SchedErr, Scheduler};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use loom_core::capability::ApiType;
use loom_core::error::{ApiError, ErrorCode};
use loom_core::ids::RequestId;
use loom_core::openai::{
    request_model, request_stream, required_caps_from_request, ModelList, SSE_DONE,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

#[derive(Clone)]
pub struct GatewayState {
    pub auth: Arc<Auth>,
    pub scheduler: Scheduler,
    pub models: crate::model_registry::ModelRegistry,
    pub usage: crate::usage::UsageWriter,
    /// Request deadline (ms) for queued/dispatched requests.
    pub request_deadline_ms: i64,
}

/// Releases the API key's inflight concurrency slot on drop unless already
/// released explicitly. Axum drops the handler future when the caller
/// disconnects — without this guard a disconnect between the rate-limit
/// commit and the finalizer spawn leaks the inflight counter forever and the
/// key eventually wedges at `max_concurrent`.
struct InflightGuard {
    auth: Arc<Auth>,
    key_id: String,
    armed: std::sync::atomic::AtomicBool,
}

impl InflightGuard {
    fn new(auth: Arc<Auth>, key_id: String) -> Self {
        Self {
            auth,
            key_id,
            armed: std::sync::atomic::AtomicBool::new(true),
        }
    }

    async fn release(&self) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.auth.rl_release(&self.key_id).await;
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let auth = self.auth.clone();
            let key_id = self.key_id.clone();
            tokio::spawn(async move {
                auth.rl_release(&key_id).await;
            });
        }
    }
}

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/images/generations", post(images_generations))
        .route(
            "/v1/images/edits",
            post(images_edits).route_layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route(
            "/v1/images/variations",
            post(images_variations)
                .route_layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route("/v1/audio/speech", post(audio_speech))
        .route(
            "/v1/audio/transcriptions",
            post(audio_transcriptions)
                .route_layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route(
            "/v1/audio/translations",
            post(audio_translations)
                .route_layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route("/v1/videos", post(videos))
        .route("/v1/models", get(models))
        .layer(middleware::from_fn_with_state(state.clone(), apikey_mw))
        .with_state(state)
}

/// Extract Bearer API key → ApiKeyCtx, enforced per-request.
async fn apikey_mw(
    State(s): State<GatewayState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    match s.auth.check_api_key(&token).await {
        Ok(Some(ctx)) => {
            req.extensions_mut().insert(ctx);
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

fn ctx(req: &Request) -> ApiKeyCtx {
    req.extensions().get::<ApiKeyCtx>().cloned().unwrap()
}

async fn chat_completions(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::Chat, req).await
}
async fn completions(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::Completions, req).await
}
async fn embeddings(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::Embeddings, req).await
}
async fn images_generations(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::Images, req).await
}
async fn images_edits(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::ImagesEdit, req).await
}
async fn images_variations(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::ImagesVariation, req).await
}
async fn audio_speech(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::AudioSpeech, req).await
}
async fn audio_transcriptions(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::AudioTranscription, req).await
}
async fn audio_translations(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::AudioTranslation, req).await
}
async fn videos(State(s): State<GatewayState>, req: Request) -> Response {
    infer(s, ApiType::Video, req).await
}

async fn models(State(s): State<GatewayState>, req: Request) -> Response {
    let c = ctx(&req);
    let all = s.models.list_detailed().await;
    let mut cards = Vec::new();
    let created = chrono::Utc::now().timestamp();
    for (name, api_type, caps, all_probed) in all {
        if !s.auth.model_allowed(&c, &name).await.unwrap_or(false) {
            continue;
        }
        let mut extra = serde_json::Map::new();
        extra.insert("api_type".into(), json!(api_type));
        extra.insert(
            "capabilities".into(),
            serde_json::to_value(&caps).unwrap_or_default(),
        );
        extra.insert("capabilities_probed".into(), json!(all_probed));
        cards.push(loom_core::openai::ModelCard {
            id: name,
            object: "model".into(),
            created,
            owned_by: "loom".into(),
            extra,
        });
    }
    Json(json!(ModelList {
        object: "list".into(),
        data: cards,
    }))
    .into_response()
}

async fn infer(s: GatewayState, api: ApiType, req: Request) -> Response {
    let c = ctx(&req);
    let request_id = RequestId::new().to_string();

    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Audio files ride multipart bodies and can be tens of MB; JSON apis
    // stay bounded at 4MB.
    let limit = if api.is_multipart() {
        128 * 1024 * 1024
    } else {
        4 * 1024 * 1024
    };
    let body_bytes = match axum::body::to_bytes(req.into_body(), limit).await {
        Ok(b) => b,
        Err(_) => return err(ErrorCode::PayloadTooLarge, "request too large", &request_id),
    };

    // Multipart bodies (audio transcription/translation) are forwarded raw;
    // the `model` form field is extracted for routing and grants only.
    let (mut body, model) = if api.is_multipart() {
        let m = match multipart_model(&body_bytes, &content_type).await {
            Some(m) => m,
            None => {
                return err(
                    ErrorCode::InvalidRequest,
                    "missing model field",
                    &request_id,
                )
            }
        };
        (Value::Null, m)
    } else {
        let b: Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => return err(ErrorCode::InvalidRequest, e.to_string(), &request_id),
        };
        let m = match request_model(&b) {
            Some(m) => m.to_string(),
            None => return err(ErrorCode::InvalidRequest, "missing model", &request_id),
        };
        (b, m)
    };

    // Scope + model permission
    // Family scope: image edits/variations are covered by the `images`
    // scope they share with generations.
    if !c
        .scopes
        .iter()
        .any(|s| s == api.family().as_str() || s == "*")
    {
        return err(
            ErrorCode::PermissionDenied,
            "scope not permitted",
            &request_id,
        );
    }
    match s.auth.model_allowed(&c, &model).await {
        Ok(true) => {}
        Ok(false) => return err(ErrorCode::ModelNotFound, "no such model", &request_id),
        Err(_) => return err(ErrorCode::Internal, "acl check", &request_id),
    }

    // Rate limits
    if let Err(e) = s.auth.rl_check(&c, &request_id).await {
        let code = if e == "concurrency_exceeded" {
            ErrorCode::ConcurrencyExceeded
        } else {
            ErrorCode::RateLimited
        };
        let mut resp = err(code, e, &request_id);
        // Clients deserve a retry hint: concurrency slots free when an
        // in-flight request ends (~seconds), RPM is a 60s sliding window —
        // the average slot interval (60s/limit) is the honest estimate.
        let secs = if e == "concurrency_exceeded" {
            1
        } else {
            (60 / c.rpm_limit.max(1)).max(1)
        };
        if let Ok(v) = secs.to_string().parse() {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, v);
        }
        return resp;
    }
    // RAII slot release: any handler drop from here on (caller disconnect at
    // an await, handler panic) still returns the inflight slot.
    let inflight = InflightGuard::new(s.auth.clone(), c.api_key_id.clone());

    // Auto-router: when the requested model is a `kind='router'` VM, classify
    // the request with the configured router model and dispatch to the chosen
    // member instead. The API key's grant on the router name covers members.
    // `required` is computed before routing so the engine can tell a member
    // that exists-but-can't-serve-this-request from a live one.
    let required = required_caps_from_request(&body);
    let mut routed: Option<crate::router_engine::RouteChoice> = None;
    let model = {
        let router_vm = if api == ApiType::Chat {
            crate::router_engine::is_router(&s.models, &model).await
        } else {
            None
        };
        if let Some(vm) = router_vm {
            match crate::router_engine::route(
                &s.scheduler,
                &s.models,
                &vm,
                &body,
                &required,
                c.priority_class,
                s.request_deadline_ms,
            )
            .await
            {
                Ok(ch) => {
                    info!(req = %request_id, router = %model, pick = %ch.model, reason = %ch.reason, "auto-routed");
                    // Member `set` overrides are merged into the request —
                    // e.g. {"reasoning_effort":"high"} for escalation or
                    // "none" for the off tier (both API forms normalized).
                    crate::router_engine::merge_set_overrides(&mut body, &ch.set);
                    let m = ch.model.clone();
                    routed = Some(ch);
                    m
                }
                Err(e) => {
                    inflight.release().await;
                    return err(ErrorCode::Unavailable, format!("router: {e}"), &request_id);
                }
            }
        } else {
            model
        }
    };

    let streaming = request_stream(&body);
    // Ask streaming backends to emit a final `usage` chunk so the client can
    // report real token counts (LM Studio, vLLM, OpenAI all honor this;
    // backends that ignore it fall back to estimation server-side).
    let body = if streaming {
        let mut b = body;
        let needs = b
            .get("stream_options")
            .and_then(|o| o.get("include_usage"))
            .is_none();
        if needs {
            // `stream_options` may legitimately be a non-object (caller sent
            // e.g. a string) — index_mut on a non-object panics, so normalize
            // to an object first.
            if !b["stream_options"].is_object() {
                b["stream_options"] = Value::Object(Default::default());
            }
            b["stream_options"]["include_usage"] = Value::Bool(true);
        }
        b
    } else {
        body
    };
    let out_bytes = if api.is_multipart() {
        body_bytes.to_vec()
    } else {
        serde_json::to_vec(&body).unwrap_or_else(|_| body_bytes.to_vec())
    };
    let now = crate::client_registry::now_ms();
    let footprint = s.scheduler.footprint(api, &body);
    crate::metrics::lease_fp(footprint as f64);
    let deadline_ts = now + s.request_deadline_ms;
    let sticky = body
        .get("user")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    let pending_content_type = if api.is_multipart() {
        content_type.clone()
    } else {
        "application/json".into()
    };

    let started = Instant::now();
    let is_sse = streaming && api.is_text_family();
    // virtual_model_id for usage_counters — resolve the dispatched model
    // (post-router member name) back to its VM row.
    let vm_id = s
        .models
        .resolve(&model)
        .await
        .map(|v| v.id)
        .unwrap_or_default();
    // Token estimate for backends that omit `usage` (some streaming modes,
    // error paths) — keeps history/stats non-zero. Real backend usage wins.
    let est_prompt = loom_core::tokens::estimate_request_tokens(&body);

    // Attempt loop: a transient post-dispatch failure (client mid
    // model-switch, transport drop, upstream 5xx) re-dispatches the request
    // on a freshly ranked member — but only while no response byte has been
    // committed to the caller. Each attempt is its own request_id/lease/
    // history row; enqueue_ts/deadline_ts stay the caller's, so FIFO order
    // and the request deadline carry across attempts.
    let max_attempts = s.scheduler.request_max_attempts();
    let mut inflight_opt = Some(inflight);
    let mut attempt: u32 = 0;
    // Members that already produced a transient failure for this logical
    // request — accumulated across attempts so a retry prefers fresh
    // members (the scheduler never lets this list empty the ranked set).
    let mut exclude_deps: Vec<String> = Vec::new();
    loop {
        attempt += 1;
        if attempt > 1 {
            if crate::client_registry::now_ms() >= deadline_ts {
                // Abandoned attempts only wrote history rows — the logical
                // request still owes its loom_requests_total tick here.
                crate::metrics::req("timeout");
                return err(
                    ErrorCode::RequestTimeout,
                    "request attempts exhausted at deadline",
                    &request_id,
                );
            }
            crate::metrics::req_retry();
            info!(req = %request_id, attempt, "re-dispatching after transient failure");
        }
        // Per-attempt clock for history duration_ms — the logical
        // `started` stays the source for request-level metrics. Attempt 1
        // keeps the caller-visible request_id as its row key so a plain
        // `WHERE request_id = <x-request-id>` lookup still lands; retries
        // get fresh ids linked via routing_trace.parent_request_id.
        let attempt_started = Instant::now();
        let attempt_rid = if attempt == 1 {
            request_id.clone()
        } else {
            RequestId::new().to_string()
        };
        let outcome = match s
            .scheduler
            .submit(PendingRequest {
                request_id: RequestId::parse(&attempt_rid).unwrap_or_default(),
                api,
                model: model.clone(),
                body: out_bytes.clone(),
                content_type: pending_content_type.clone(),
                required: required.clone(),
                project_id: c.project_id.clone(),
                prio: c.priority_class,
                enqueue_ts: now,
                deadline_ts,
                sticky: sticky.clone(),
                footprint_tokens: footprint,
                exclude_deps: exclude_deps.clone(),
            })
            .await
        {
            Ok(o) => o,
            // Safety net: candidate-level ClientOffline is absorbed inside
            // try_dispatch's member loop today, but if a future path lets it
            // escape it is the same transient class as a post-dispatch
            // transport drop — worth a fresh member, not a terminal 503.
            // Everything else escaping submit is structural (no candidates,
            // context too large, acl, cancelled, deadline) and stays terminal.
            Err(SchedErr::ClientOffline) if attempt < max_attempts => continue,
            Err(e) => return sched_err(e, &request_id),
        };
        let RequestOutcomeParts {
            mut rx,
            upstream_status,
            end_rx,
            mut trace,
            dep,
            client,
            lease: _lease,
            cancel,
            queue_wait_ms,
        } = split(outcome);
        trace["attempt"] = json!(attempt);
        trace["parent_request_id"] = json!(request_id);
        let rec = RecorderCtx {
            rid: attempt_rid.clone(),
            api,
            key_id: c.api_key_id.clone(),
            project_id: c.project_id.clone(),
            vm_id: vm_id.clone(),
            dep: dep.clone(),
            client,
            queue_wait_ms,
            est_prompt,
            started,
            attempt_started,
            usage: s.usage.clone(),
            auth: s.auth.clone(),
            trace,
            count_metrics: false,
        };

        // Buffered families (media apis + non-streaming text): the caller
        // sees nothing until the full upstream body arrived, so a retry is
        // safe for any transient classification — not just pre-byte.
        if !is_sse {
            let mut buf = Vec::new();
            let mut failure: Option<SchedErr> = None;
            while let Some(item) = rx.recv().await {
                match item {
                    Ok(b) => buf.extend_from_slice(&b),
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            let us = upstream_status.load(std::sync::atomic::Ordering::Relaxed);
            match classify_buffered(failure.as_ref(), us, buf.is_empty()) {
                BufferedVerdict::Retryable if attempt < max_attempts => {
                    // Slim recorder: usage row for the failed attempt, no
                    // request metrics, guard stays for the next attempt.
                    spawn_recorder(end_rx, None, rec);
                    exclude_deps.push(dep);
                    cancel.cancel();
                    drop(rx);
                    continue;
                }
                BufferedVerdict::Retryable => {
                    // Attempts exhausted — degrade to the honest last outcome
                    // instead of inventing a new error shape.
                    spawn_recorder(
                        end_rx,
                        inflight_opt.take(),
                        RecorderCtx {
                            count_metrics: true,
                            ..rec
                        },
                    );
                    if let Some(e) = failure {
                        let (code, msg) = sched_err_shape(e);
                        return err(code, msg, &request_id);
                    }
                    if us == 0 || buf.is_empty() {
                        return err(
                            ErrorCode::UpstreamError,
                            "upstream closed before producing a response",
                            &request_id,
                        );
                    }
                    return buffered_response(
                        api,
                        us,
                        buf,
                        &request_id,
                        routed.as_ref(),
                        &body,
                        attempt,
                    );
                }
                BufferedVerdict::Fail => {
                    spawn_recorder(
                        end_rx,
                        inflight_opt.take(),
                        RecorderCtx {
                            count_metrics: true,
                            ..rec
                        },
                    );
                    let (code, msg) = sched_err_shape(
                        failure.unwrap_or_else(|| SchedErr::Backend("stream closed".into())),
                    );
                    return err(code, msg, &request_id);
                }
                BufferedVerdict::Commit => {
                    spawn_recorder(
                        end_rx,
                        inflight_opt.take(),
                        RecorderCtx {
                            count_metrics: true,
                            ..rec
                        },
                    );
                    return buffered_response(
                        api,
                        us,
                        buf,
                        &request_id,
                        routed.as_ref(),
                        &body,
                        attempt,
                    );
                }
            }
        }

        // SSE: the first item decides — a chunk commits headers + body, a
        // pre-byte retryable Err/None may re-dispatch. After the first byte
        // the request is committed and failures surface as stream envelopes.
        match rx.recv().await {
            Some(Ok(first)) => {
                spawn_recorder(
                    end_rx,
                    inflight_opt.take(),
                    RecorderCtx {
                        count_metrics: true,
                        ..rec
                    },
                );
                let stream = sse_stream(first, rx, started, request_id.clone(), cancel);
                info!(req = %request_id, model = %model, attempt, "request dispatched");
                return sse_response(stream, &request_id, routed.as_ref(), attempt);
            }
            Some(Err(e)) => {
                if matches!(e, SchedErr::Retryable(_)) && attempt < max_attempts {
                    spawn_recorder(end_rx, None, rec);
                    exclude_deps.push(dep);
                    cancel.cancel();
                    drop(rx);
                    continue;
                }
                spawn_recorder(
                    end_rx,
                    inflight_opt.take(),
                    RecorderCtx {
                        count_metrics: true,
                        ..rec
                    },
                );
                return sse_response(
                    sse_error_envelope(&e, &request_id),
                    &request_id,
                    routed.as_ref(),
                    attempt,
                );
            }
            None => {
                if attempt < max_attempts {
                    spawn_recorder(end_rx, None, rec);
                    exclude_deps.push(dep);
                    cancel.cancel();
                    drop(rx);
                    continue;
                }
                spawn_recorder(
                    end_rx,
                    inflight_opt.take(),
                    RecorderCtx {
                        count_metrics: true,
                        ..rec
                    },
                );
                return sse_response(
                    sse_error_envelope(
                        &SchedErr::Retryable("stream closed before response".into()),
                        &request_id,
                    ),
                    &request_id,
                    routed.as_ref(),
                    attempt,
                );
            }
        }
    }
}

/// Response content type for buffered media replies. For speech the caller's
/// `response_format` selects the codec (mp3 default), everything else is JSON.
fn media_content_type(api: ApiType, body: &Value) -> &'static str {
    if api == ApiType::AudioSpeech {
        match body["response_format"].as_str().unwrap_or("mp3") {
            "opus" => "audio/opus",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            "wav" => "audio/wav",
            "pcm" => "audio/pcm",
            _ => "audio/mpeg",
        }
    } else {
        "application/json"
    }
}

/// Extract the `model` form field from a raw multipart request body —
/// needed for routing and grant checks before the body is forwarded as-is.
async fn multipart_model(body: &[u8], content_type: &str) -> Option<String> {
    let boundary = multer::parse_boundary(content_type).ok()?;
    let bytes = Bytes::copy_from_slice(body);
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(bytes) });
    let mut mp = multer::Multipart::new(stream, boundary);
    while let Ok(Some(field)) = mp.next_field().await {
        if field.name() == Some("model") {
            return field.text().await.ok().map(|s| s.trim().to_string());
        }
    }
    None
}

struct RequestOutcomeParts {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, SchedErr>>,
    upstream_status: std::sync::Arc<std::sync::atomic::AtomicU32>,
    end_rx: tokio::sync::oneshot::Receiver<EndMeta>,
    trace: serde_json::Value,
    dep: String,
    client: String,
    lease: String,
    cancel: tokio_util::sync::CancellationToken,
    queue_wait_ms: i64,
}

fn split(o: crate::scheduler::RequestOutcome) -> RequestOutcomeParts {
    RequestOutcomeParts {
        rx: o.response_rx,
        upstream_status: o.upstream_status,
        end_rx: o.end_rx,
        trace: o.routing_trace,
        dep: o.deployment_id,
        client: o.client_id,
        lease: o.lease_id,
        cancel: o.cancel,
        queue_wait_ms: o.queue_wait_ms,
    }
}

/// Per-attempt bookkeeping for the usage/history recorder. The committed
/// (or terminally failed) attempt additionally carries the inflight guard
/// and bumps request-level metrics; abandoned retry attempts only write
/// their own history row.
struct RecorderCtx {
    rid: String,
    api: ApiType,
    key_id: String,
    project_id: String,
    vm_id: String,
    dep: String,
    client: String,
    queue_wait_ms: i64,
    est_prompt: u32,
    /// Logical request start — request-level metrics measure the caller's
    /// full wait including retries.
    started: Instant,
    /// This attempt's start — the history row's duration_ms is per-attempt.
    attempt_started: Instant,
    usage: crate::usage::UsageWriter,
    auth: Arc<Auth>,
    trace: serde_json::Value,
    count_metrics: bool,
}

/// Awaits RequestEnd (or the bridge's synthetic fallback), records
/// usage/history, charges real tokens to the rate limiter, and — when the
/// attempt produced the caller-visible response — releases the inflight
/// slot and counts request metrics. ALWAYS runs, also when the HTTP caller
/// disconnects mid-attempt: axum drops the handler future but this task
/// lives on.
fn spawn_recorder(
    end_rx: tokio::sync::oneshot::Receiver<EndMeta>,
    guard: Option<InflightGuard>,
    ctx: RecorderCtx,
) {
    let RecorderCtx {
        rid,
        api,
        key_id,
        project_id,
        vm_id,
        dep,
        client,
        queue_wait_ms,
        est_prompt,
        started,
        attempt_started,
        usage,
        auth,
        trace,
        count_metrics,
    } = ctx;
    let rid_f = rid.clone();
    tokio::spawn(async move {
        // The bridge resolves `ends` within ~15s of stream end; the 24h
        // bound below is a last-resort catch for a bridge task that died
        // without running its tail — bounds the leak, never hits real
        // requests.
        let meta = tokio::time::timeout(std::time::Duration::from_secs(24 * 3600), end_rx)
            .await
            .map(|r| r.unwrap_or_default())
            .unwrap_or_else(|_| {
                tracing::warn!(req = %rid_f, "recorder end_rx timed out — resolving synthetically");
                EndMeta::default()
            });
        let prompt_tokens = if meta.prompt_tokens > 0 {
            meta.prompt_tokens
        } else {
            est_prompt
        };
        let completion_tokens = if meta.completion_tokens > 0 {
            meta.completion_tokens
        } else {
            // Heuristic: ~1 output token per 25ms of decode after TTFT.
            (meta.duration_ms.saturating_sub(meta.ttft_ms).max(0) / 25) as u32
        };
        let status = if meta.status.is_empty() {
            "aborted".into()
        } else {
            meta.status
        };
        if count_metrics {
            crate::metrics::req(&status);
            crate::metrics::queue_wait(queue_wait_ms as f64 / 1000.0);
            crate::metrics::duration(started.elapsed().as_secs_f64());
            crate::metrics::req_tokens((prompt_tokens + completion_tokens) as f64);
        }
        usage
            .record(crate::usage::UsageRecord {
                request_id: rid_f.clone(),
                api_type: api.as_str().into(),
                api_key_id: key_id.clone(),
                project_id: project_id.clone(),
                virtual_model_id: vm_id.clone(),
                deployment_id: dep,
                client_id: client,
                status,
                queue_wait_ms,
                ttft_ms: meta.ttft_ms,
                ttfrt_ms: meta.ttfrt_ms,
                duration_ms: attempt_started.elapsed().as_millis() as i64,
                prompt_tokens,
                completion_tokens,
                error_code: meta.error_code,
                routing_trace: trace,
            })
            .await;
        auth.rl_charge_tpm(&key_id, &rid_f, (prompt_tokens + completion_tokens) as u64)
            .await;
        if let Some(g) = guard {
            g.release().await;
        }
    });
}

/// Buffered-path retry classification. Everything here ran to stream end
/// with zero bytes committed to the caller, so retrying is safe — the
/// question is only whether another member could produce a different
/// outcome.
#[derive(Debug, PartialEq, Eq)]
enum BufferedVerdict {
    /// Deliver the buffered upstream response (status + body preserved).
    Commit,
    /// Transient failure — worth a fresh member. `us>=500` and `us==0`
    /// (stream aborted before its End frame) are transport-shaped;
    /// `200` + empty body is a zombie generation (legit buffered responses
    /// are never empty).
    Retryable,
    /// Deterministic failure — relay the error as-is.
    Fail,
}

fn classify_buffered(err: Option<&SchedErr>, us: u32, buf_empty: bool) -> BufferedVerdict {
    match err {
        Some(SchedErr::Retryable(_)) => BufferedVerdict::Retryable,
        Some(_) => BufferedVerdict::Fail,
        None => {
            if us >= 500 || us == 0 || (us == 200 && buf_empty) {
                BufferedVerdict::Retryable
            } else {
                BufferedVerdict::Commit
            }
        }
    }
}

/// Buffered response for media apis (200 + inferred content type) and
/// non-streaming text (real upstream status + JSON body).
fn buffered_response(
    api: ApiType,
    us: u32,
    buf: Vec<u8>,
    request_id: &str,
    routed: Option<&crate::router_engine::RouteChoice>,
    body: &Value,
    attempt: u32,
) -> Response {
    let (status, content_type) = if api.is_text_family() {
        let code = if (100..=599).contains(&us) {
            us as u16
        } else {
            200
        };
        (
            StatusCode::from_u16(code).unwrap_or(StatusCode::OK),
            "application/json",
        )
    } else if (400..=599).contains(&us) {
        // Media apis still return real upstream error statuses: the body
        // then is the backend's JSON error envelope, not media bytes — a
        // faked `200 audio/mpeg` would hand callers a corrupt file.
        (
            StatusCode::from_u16(us as u16).unwrap_or(StatusCode::BAD_GATEWAY),
            "application/json",
        )
    } else {
        (StatusCode::OK, media_content_type(api, body))
    };
    let mut rb = Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("x-request-id", request_id)
        .header("x-loom-attempts", attempt.to_string());
    if let Some(ch) = routed {
        rb = rb
            .header("x-loom-routed-model", &ch.model)
            .header("x-loom-route-reason", &ch.reason);
    }
    rb.body(Body::from(buf))
        .unwrap_or_else(|_| err(ErrorCode::Internal, "response build", request_id))
}

/// Committed SSE body: yields `first` (already in hand — its arrival
/// decided the commit) then drains the bridge until end/error.
fn sse_stream(
    first: Bytes,
    mut rx: tokio::sync::mpsc::Receiver<Result<Bytes, SchedErr>>,
    started: Instant,
    rid: String,
    cancel: tokio_util::sync::CancellationToken,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    async_stream::stream! {
        crate::metrics::ttft(started.elapsed().as_secs_f64());
        let mut saw_done = first.windows(6).any(|w| w == b"[DONE]");
        yield Ok::<Bytes, std::io::Error>(first);
        loop {
            match rx.recv().await {
                Some(Ok(b)) => {
                    if b.windows(6).any(|w| w == b"[DONE]") {
                        saw_done = true;
                    }
                    yield Ok::<Bytes, std::io::Error>(b);
                }
                Some(Err(e)) => {
                    let msg = match &e {
                        SchedErr::Retryable(m) | SchedErr::Backend(m) => m.clone(),
                        other => other.to_string(),
                    };
                    let env = ApiError::new(ErrorCode::UpstreamError, msg)
                        .with_request_id(&rid)
                        .envelope();
                    let line =
                        format!("data: {}\n\n", serde_json::to_string(&env).unwrap_or_default());
                    yield Ok(Bytes::from(line));
                    break;
                }
                None => break,
            }
        }
        if !saw_done {
            yield Ok(Bytes::from(SSE_DONE));
        }
        let _ = cancel;
    }
}

/// Single error-envelope chunk + [DONE] for SSE attempts that failed
/// without a first byte (envelope over silent truncation).
fn sse_error_envelope(
    e: &SchedErr,
    rid: &str,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let msg = match e {
        SchedErr::Retryable(m) | SchedErr::Backend(m) => m.clone(),
        other => other.to_string(),
    };
    let env = ApiError::new(ErrorCode::UpstreamError, msg)
        .with_request_id(rid)
        .envelope();
    let line = format!(
        "data: {}\n\n",
        serde_json::to_string(&env).unwrap_or_default()
    );
    futures_util::stream::iter([
        Ok::<Bytes, std::io::Error>(Bytes::from(line)),
        Ok(Bytes::from(SSE_DONE)),
    ])
}

fn sse_response<S>(
    stream: S,
    request_id: &str,
    routed: Option<&crate::router_engine::RouteChoice>,
    attempt: u32,
) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let mut rb = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("x-request-id", request_id)
        .header("x-loom-attempts", attempt.to_string());
    if let Some(ch) = routed {
        rb = rb
            .header("x-loom-routed-model", &ch.model)
            .header("x-loom-route-reason", &ch.reason);
    }
    rb.body(Body::from_stream(stream))
        .unwrap_or_else(|_| err(ErrorCode::Internal, "response build", request_id))
}

/// SchedErr → caller-facing error shape, without side effects. Post-dispatch
/// paths pair it with `err()` — their committed-attempt recorder already
/// counts loom_requests_total, so `sched_err`'s own count would double up.
fn sched_err_shape(e: SchedErr) -> (ErrorCode, String) {
    match e {
        SchedErr::QueueFull => (ErrorCode::Overloaded, "queue full".into()),
        SchedErr::NoCapacity => (ErrorCode::Overloaded, "no capacity".into()),
        SchedErr::NoCandidates => (ErrorCode::Unavailable, "no deployment available".into()),
        SchedErr::ClientOffline => (ErrorCode::Unavailable, "client offline".into()),
        SchedErr::AllUnavailable => (ErrorCode::Unavailable, "all deployments unavailable".into()),
        SchedErr::Rejected(r) => (ErrorCode::Unavailable, r),
        SchedErr::Timeout => (ErrorCode::RequestTimeout, "request timeout".into()),
        SchedErr::Cancelled => (ErrorCode::Cancelled, "cancelled".into()),
        SchedErr::ContextTooLarge => (
            ErrorCode::InvalidRequest,
            "request exceeds the context capacity of every deployment".into(),
        ),
        SchedErr::ApiTypeMismatch => (
            ErrorCode::InvalidRequest,
            "model does not serve this api type".into(),
        ),
        SchedErr::Backend(m) | SchedErr::Retryable(m) => (ErrorCode::UpstreamError, m),
    }
}

fn sched_err(e: SchedErr, rid: &str) -> Response {
    // Pre-dispatch failures never reach the usage finalizer — count them here
    // so loom_requests_total covers the full request lifecycle.
    crate::metrics::req(match &e {
        SchedErr::QueueFull => "queue_full",
        SchedErr::NoCapacity => "no_capacity",
        SchedErr::NoCandidates => "no_candidates",
        SchedErr::ClientOffline => "client_offline",
        SchedErr::AllUnavailable => "unavailable",
        SchedErr::Rejected(_) => "rejected",
        SchedErr::Timeout => "timeout",
        SchedErr::Cancelled => "cancelled",
        SchedErr::ContextTooLarge => "context_too_large",
        SchedErr::ApiTypeMismatch => "api_type_mismatch",
        SchedErr::Backend(_) => "backend_error",
        SchedErr::Retryable(_) => "retry_exhausted",
    });
    let (code, msg) = sched_err_shape(e);
    err(code, msg, rid)
}

fn err(code: ErrorCode, msg: impl Into<String>, rid: &str) -> Response {
    let env = ApiError::new(code, msg).with_request_id(rid).envelope();
    (
        StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(env),
    )
        .into_response()
}

// header passthrough marker (kept minimal)
fn _unused(_h: &HeaderMap) {
    warn!("unused")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(m: &str) -> SchedErr {
        SchedErr::Backend(m.into())
    }
    fn re(m: &str) -> SchedErr {
        SchedErr::Retryable(m.into())
    }

    #[test]
    fn classify_buffered_matrix() {
        // Error items decide by their SchedErr variant.
        assert_eq!(
            classify_buffered(Some(&re("conn reset")), 0, true),
            BufferedVerdict::Retryable
        );
        assert_eq!(
            classify_buffered(Some(&be("bad request")), 200, false),
            BufferedVerdict::Fail
        );
        // No error item: upstream status + body shape decide.
        assert_eq!(
            classify_buffered(None, 503, false),
            BufferedVerdict::Retryable,
            "proxied 5xx body is transient — another member may serve it"
        );
        assert_eq!(
            classify_buffered(None, 0, false),
            BufferedVerdict::Retryable,
            "us==0 means the stream aborted before its End frame"
        );
        assert_eq!(
            classify_buffered(None, 200, true),
            BufferedVerdict::Retryable,
            "200 + empty body is a zombie generation"
        );
        assert_eq!(classify_buffered(None, 200, false), BufferedVerdict::Commit);
        assert_eq!(
            classify_buffered(None, 404, false),
            BufferedVerdict::Commit,
            "deterministic upstream statuses pass through untouched"
        );
        assert_eq!(
            classify_buffered(None, 429, false),
            BufferedVerdict::Commit,
            "upstream rate limits are caller-visible, not retryable"
        );
    }
}
