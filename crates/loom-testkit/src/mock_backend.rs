//! Mock OpenAI-compatible inference backend for tests.
//! Configurable latency, chunk counts, error injection and cancel detection.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use loom_core::openai::{sse_data_line, SSE_DONE};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone)]
pub struct MockBehavior {
    /// Time-to-first-chunk delay.
    pub ttft: Duration,
    /// Delay between streamed chunks.
    pub chunk_delay: Duration,
    /// Number of streamed content chunks for streaming requests.
    pub stream_chunks: usize,
    /// If set, respond with this status + error JSON for inference endpoints.
    pub fail_status: Option<u16>,
    /// Simulated tokens per chunk.
    pub tokens_per_chunk: u32,
    /// Health status.
    pub healthy: bool,
}

impl Default for MockBehavior {
    fn default() -> Self {
        Self {
            ttft: Duration::from_millis(5),
            chunk_delay: Duration::from_millis(5),
            stream_chunks: 4,
            fail_status: None,
            tokens_per_chunk: 8,
            healthy: true,
        }
    }
}

#[derive(Clone)]
pub struct MockBackend {
    state: Arc<MockState>,
}

struct MockState {
    behavior: std::sync::RwLock<MockBehavior>,
    model: String,
    in_flight: AtomicUsize,
    total_requests: AtomicUsize,
    cancelled: AtomicUsize,
    completed: AtomicUsize,
}

impl MockBackend {
    pub fn new(model: &str) -> Self {
        Self {
            state: Arc::new(MockState {
                behavior: std::sync::RwLock::new(MockBehavior::default()),
                model: model.into(),
                in_flight: AtomicUsize::new(0),
                total_requests: AtomicUsize::new(0),
                cancelled: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
            }),
        }
    }

    pub fn router(&self) -> Router {
        let st = self.state.clone();
        Router::new()
            .route("/v1/chat/completions", post(chat))
            .route("/v1/completions", post(completions))
            .route("/v1/embeddings", post(embeddings))
            .route("/v1/models", get(models))
            .route("/health", get(health))
            .route("/metrics", get(|| async { "# mock metrics\n" }))
            .with_state(st)
    }

    /// Spawn on an ephemeral port; returns the bound URL and a shutdown handle.
    pub async fn spawn(model: &str) -> (String, Self, tokio::task::JoinHandle<()>) {
        let backend = Self::new(model);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = backend.router();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        (format!("http://{addr}"), backend, handle)
    }

    pub fn set_behavior(&self, b: MockBehavior) {
        *self.state.behavior.write().unwrap() = b;
    }
    pub fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::SeqCst)
    }
    pub fn total_requests(&self) -> usize {
        self.state.total_requests.load(Ordering::SeqCst)
    }
    pub fn cancelled(&self) -> usize {
        self.state.cancelled.load(Ordering::SeqCst)
    }
    pub fn completed(&self) -> usize {
        self.state.completed.load(Ordering::SeqCst)
    }
}

fn behavior(st: &MockState) -> MockBehavior {
    st.behavior.read().unwrap().clone()
}

async fn chat(
    State(st): State<Arc<MockState>>,
    _h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    st.total_requests.fetch_add(1, Ordering::SeqCst);
    let b = behavior(&st);
    if !b.healthy {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"backend unhealthy"}}))).into_response();
    }
    if let Some(code) = b.fail_status {
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return (status, Json(json!({"error":{"message":"injected failure","code":code}}))).into_response();
    }

    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or(&st.model).to_string();
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    st.in_flight.fetch_add(1, Ordering::SeqCst);
    let st2 = st.clone();

    if stream {
        let (tx, rx) = mpsc::channel::<Result<String, std::io::Error>>(16);
        tokio::spawn(async move {
            struct Guard(Arc<MockState>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    let n = self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
                    if n == 0 {
                        self.0.in_flight.store(0, Ordering::SeqCst);
                    }
                }
            }
            let _g = Guard(st2.clone());
            tokio::time::sleep(b.ttft).await;
            for i in 0..b.stream_chunks {
                let finish = if i + 1 == b.stream_chunks { Some("stop") } else { None };
                let mut chunk = json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": model,
                    "choices": [{"index":0,"delta":{"content": format!("tok{i} ")},"finish_reason":finish}],
                });
                if finish.is_some() {
                    chunk["usage"] = json!({
                        "prompt_tokens": 10,
                        "completion_tokens": (b.stream_chunks as u32) * b.tokens_per_chunk,
                        "total_tokens": 10 + (b.stream_chunks as u32) * b.tokens_per_chunk,
                    });
                }
                if tx.send(Ok(sse_data_line(&chunk))).await.is_err() {
                    st2.cancelled.fetch_add(1, Ordering::SeqCst);
                    return; // downstream gone -> abort
                }
                tokio::time::sleep(b.chunk_delay).await;
            }
            let _ = tx.send(Ok(SSE_DONE.to_string())).await;
            st2.completed.fetch_add(1, Ordering::SeqCst);
        });
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(ReceiverStream::new(rx)))
            .unwrap()
    } else {
        tokio::time::sleep(b.ttft).await;
        st.in_flight.fetch_sub(1, Ordering::SeqCst);
        st.completed.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 1,
            "model": model,
            "choices": [{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens":10,"completion_tokens":3,"total_tokens":13},
        }))
        .into_response()
    }
}

async fn completions(State(st): State<Arc<MockState>>, Json(body): Json<Value>) -> Response {
    // reuse chat-shaped behavior
    chat(State(st), HeaderMap::new(), Json(body)).await
}

async fn embeddings(State(st): State<Arc<MockState>>, Json(_body): Json<Value>) -> Response {
    st.total_requests.fetch_add(1, Ordering::SeqCst);
    st.completed.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "object": "list",
        "data": [{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],
        "model": st.model,
        "usage": {"prompt_tokens":5,"total_tokens":5},
    }))
    .into_response()
}

async fn models(State(st): State<Arc<MockState>>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{"id": st.model, "object":"model","created":1,"owned_by":"mock"}]
    }))
}

async fn health(State(st): State<Arc<MockState>>) -> StatusCode {
    if behavior(&st).healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
