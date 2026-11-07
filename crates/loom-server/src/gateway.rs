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

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/images/generations", post(images_generations))
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
    let all = s.models.list().await;
    let mut allowed = Vec::new();
    for m in all {
        if s.auth.model_allowed(&c, &m).await.unwrap_or(false) {
            allowed.push(m);
        }
    }
    Json(json!(ModelList::of(allowed, "loom"))).into_response()
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
    if !c.scopes.iter().any(|s| s == api.as_str() || s == "*") {
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
        return err(code, e, &request_id);
    }

    // Auto-router: when the requested model is a `kind='router'` VM, classify
    // the request with the configured router model and dispatch to the chosen
    // member instead. The API key's grant on the router name covers members.
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
                    s.auth.rl_release(&c.api_key_id).await;
                    return err(ErrorCode::Unavailable, format!("router: {e}"), &request_id);
                }
            }
        } else {
            model
        }
    };

    let required = required_caps_from_request(&body);
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
    let pending = PendingRequest {
        request_id: RequestId::parse(&request_id).unwrap_or_default(),
        api,
        model: model.clone(),
        body: out_bytes,
        content_type: if api.is_multipart() {
            content_type.clone()
        } else {
            "application/json".into()
        },
        required,
        project_id: c.project_id.clone(),
        prio: c.priority_class,
        enqueue_ts: now,
        deadline_ts: now + s.request_deadline_ms,
    };

    let started = Instant::now();
    let outcome = match s.scheduler.submit(pending).await {
        Ok(o) => o,
        Err(e) => {
            s.auth.rl_release(&c.api_key_id).await;
            return sched_err(e, &request_id);
        }
    };

    let RequestOutcomeParts {
        rx,
        end_rx,
        trace,
        dep,
        client,
        lease: _lease,
        cancel,
    } = split(outcome);
    let rid = request_id.clone();
    let key_id = c.api_key_id.clone();
    let usage = s.usage.clone();
    let auth2 = s.auth.clone();
    let started2 = started;
    let is_sse = streaming;
    let cancel2 = cancel.clone();
    // Token estimate for backends that omit `usage` (some streaming modes,
    // error paths) — keeps history/stats non-zero. Real backend usage wins.
    let est_prompt = loom_core::tokens::estimate_request_tokens(&body);

    // Finalizer task: ALWAYS runs (also when the HTTP caller disconnects —
    // axum drops the body stream but this task lives on). Awaits RequestEnd,
    // records usage/history, releases the rate-limit inflight slot, and
    // drops the lease if anything is still held.
    {
        let rid_f = rid.clone();
        let cancel_f = cancel.clone();
        tokio::spawn(async move {
            let meta = end_rx.await.unwrap_or_default();
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
            usage
                .record(crate::usage::UsageRecord {
                    request_id: rid_f,
                    api_key_id: key_id.clone(),
                    project_id: String::new(),
                    virtual_model_id: String::new(),
                    deployment_id: dep,
                    client_id: client,
                    status: if meta.status.is_empty() {
                        "aborted".into()
                    } else {
                        meta.status
                    },
                    queue_wait_ms: 0,
                    ttft_ms: meta.ttft_ms,
                    duration_ms: started2.elapsed().as_millis() as i64,
                    prompt_tokens,
                    completion_tokens,
                    error_code: meta.error_code,
                    routing_trace: trace,
                })
                .await;
            auth2.rl_release(&key_id).await;
            let _ = cancel_f;
        });
    }

    // Media apis return single-buffer replies (binary audio, image/video
    // JSON): collect the full upstream body, then answer with the inferred
    // content type — the upstream media type is not relayed over the data
    // stream, and these endpoints never stream SSE.
    if !api.is_text_family() {
        let mut rx = rx;
        let mut buf = Vec::new();
        while let Some(chunk) = rx.recv().await {
            match chunk {
                Ok(b) => buf.extend_from_slice(&b),
                Err(e) => {
                    return err(ErrorCode::UpstreamError, e.to_string(), &request_id);
                }
            }
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", media_content_type(api, &body))
            .header("x-request-id", &request_id)
            .body(Body::from(buf))
            .unwrap_or_else(|_| err(ErrorCode::Internal, "response build", &request_id));
    }

    let stream = async_stream::stream! {
        let mut rx = rx;
        let mut first = true;
        let mut saw_done = false;
        loop {
            match rx.recv().await {
                Some(Ok(b)) => {
                    if first {
                        first = false;
                        crate::metrics::ttft(started2.elapsed().as_secs_f64());
                    }
                    if is_sse && b.windows(6).any(|w| w == b"[DONE]") {
                        saw_done = true;
                    }
                    yield Ok::<Bytes, std::io::Error>(b);
                }
                Some(Err(e)) => {
                    let env = ApiError::new(ErrorCode::UpstreamError, e.to_string())
                        .with_request_id(&rid)
                        .envelope();
                    let line = if is_sse {
                        format!("data: {}\n\n", serde_json::to_string(&env).unwrap_or_default())
                    } else {
                        serde_json::to_string(&env).unwrap_or_default()
                    };
                    yield Ok(Bytes::from(line));
                    break;
                }
                None => break,
            }
        }
        if is_sse && !saw_done {
            yield Ok(Bytes::from(SSE_DONE));
        }
        let _ = cancel2;
    };

    let content_type = if is_sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    info!(req = %request_id, model = %model, streaming = is_sse, "request dispatched");
    let mut rb = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("x-request-id", &request_id);
    if let Some(ch) = &routed {
        rb = rb
            .header("x-loom-routed-model", &ch.model)
            .header("x-loom-route-reason", &ch.reason);
    }
    rb.body(Body::from_stream(stream))
        .unwrap_or_else(|_| err(ErrorCode::Internal, "response build", &request_id))
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
    end_rx: tokio::sync::oneshot::Receiver<EndMeta>,
    trace: serde_json::Value,
    dep: String,
    client: String,
    lease: String,
    cancel: tokio_util::sync::CancellationToken,
}

fn split(o: crate::scheduler::RequestOutcome) -> RequestOutcomeParts {
    RequestOutcomeParts {
        rx: o.response_rx,
        end_rx: o.end_rx,
        trace: o.routing_trace,
        dep: o.deployment_id,
        client: o.client_id,
        lease: o.lease_id,
        cancel: o.cancel,
    }
}

fn sched_err(e: SchedErr, rid: &str) -> Response {
    let (code, msg): (ErrorCode, String) = match e {
        SchedErr::QueueFull => (ErrorCode::Overloaded, "queue full".into()),
        SchedErr::NoCapacity => (ErrorCode::Overloaded, "no capacity".into()),
        SchedErr::NoCandidates => (ErrorCode::Unavailable, "no deployment available".into()),
        SchedErr::ClientOffline => (ErrorCode::Unavailable, "client offline".into()),
        SchedErr::Rejected(r) => (ErrorCode::Unavailable, r),
        SchedErr::Timeout => (ErrorCode::RequestTimeout, "request timeout".into()),
        SchedErr::Cancelled => (ErrorCode::Cancelled, "cancelled".into()),
        SchedErr::Backend(m) => (ErrorCode::UpstreamError, m),
    };
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
