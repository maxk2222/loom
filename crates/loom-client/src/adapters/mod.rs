//! Backend adapters: uniform interface over heterogeneous inference servers.
//! The generic OpenAI adapter is always usable; specific adapters add probing
//! and health semantics.

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::Value;
use std::pin::Pin;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub mod anthropic;
pub mod azure;
pub mod generic;
pub mod llamacpp;
pub mod ollama;
pub mod vllm;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, AdapterError>> + Send>>;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("timeout")]
    Timeout,
    #[error("backend returned {status}: {message}")]
    Upstream { status: u16, message: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("cancelled")]
    Cancelled,
    #[error("io: {0}")]
    Io(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded,
    Down,
    Unknown,
}

impl Health {
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Down => "down",
            Health::Unknown => "unknown",
        }
    }
}

/// A forwarded upstream response: status + raw body stream (SSE bytes or JSON).
pub struct BackendResponse {
    pub status: u16,
    pub is_event_stream: bool,
    pub stream: ByteStream,
    /// Content-Type of the upstream response.
    pub content_type: String,
}

#[async_trait]
pub trait BackendAdapter: Send + Sync {
    fn backend_type(&self) -> BackendType;

    /// List models the backend currently serves.
    async fn list_models(&self) -> Result<Vec<String>, AdapterError>;

    /// Models loaded in memory and servable immediately (JIT engines can
    /// evict between requests). `None` = cannot determine → treated as
    /// loaded (permissive). An empty `Some` means "known: nothing loaded".
    async fn loaded_models(&self) -> Option<Vec<String>> {
        None
    }

    /// Candidate set for `model_name == "auto"` binding. Defaults to the
    /// resident set; adapters whose registry distinguishes model kinds
    /// (LM Studio) exclude non-chat models — an auto deployment bound to
    /// an embedding model could never serve a chat request.
    async fn bindable_models(&self) -> Option<Vec<String>> {
        self.loaded_models().await
    }

    /// Probe capabilities of a specific model on this backend.
    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError>;

    /// Health check of the backend endpoint.
    async fn health(&self) -> Health;

    /// Warm up / load a model into memory (JIT engines). No-op default:
    /// a 1-token chat request triggers load on OpenAI-compatible backends.
    async fn warm_up(&self, model: &str) -> Result<(), AdapterError> {
        let _ = model;
        Ok(())
    }

    /// Forward an OpenAI request body to the backend; returns the raw response.
    async fn forward(
        &self,
        api: ApiType,
        model: &str,
        body: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError>;

    /// Unload a model from backend memory (pressure offload). Default:
    /// unsupported — remote APIs and backends without unload semantics simply
    /// keep the model; the tier-2 gate still protects local resources.
    async fn unload(&self, model: &str) -> Result<(), AdapterError> {
        let _ = model;
        Err(AdapterError::Protocol(
            "unload not supported by this backend".into(),
        ))
    }

    /// Forward a raw multipart body (audio transcriptions/translations). The
    /// `model` form field is rewritten to the deployment's physical model.
    /// Default: unsupported — most backends serve no multipart endpoints.
    async fn forward_multipart(
        &self,
        api: ApiType,
        model: &str,
        body: &[u8],
        content_type: &str,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        let _ = (api, model, body, content_type, cancel);
        Err(AdapterError::Protocol(
            "multipart not supported by this backend".into(),
        ))
    }
}

/// Build an adapter for a backend config.
pub fn make_adapter(cfg: &BackendConfig, http: reqwest::Client) -> Box<dyn BackendAdapter> {
    match cfg.backend_type {
        BackendType::Vllm => Box::new(vllm::VllmAdapter::new(cfg.clone(), http)),
        BackendType::Ollama => Box::new(ollama::OllamaAdapter::new(cfg.clone(), http)),
        BackendType::GenericOpenAi => Box::new(generic::GenericAdapter::new(cfg.clone(), http)),
        BackendType::AzureOpenAi => Box::new(azure::AzureAdapter::new(cfg.clone(), http)),
        BackendType::Anthropic => Box::new(anthropic::AnthropicAdapter::new(cfg.clone(), http)),
        BackendType::LlamaCpp => Box::new(llamacpp::LlamaCppAdapter::new(cfg.clone(), http)),
    }
}

/// Shared helpers for OpenAI-compatible adapters.
pub(crate) mod common {
    use super::*;
    use futures_util::StreamExt;

    /// Forward `body` to `{endpoint}{api.path()}` and return the raw response.
    #[allow(clippy::too_many_arguments)]
    pub async fn forward_openai(
        http: &reqwest::Client,
        endpoint: &str,
        api_key: &str,
        api: ApiType,
        model: &str,
        mut body: Value,
        cancel: CancellationToken,
        timeout: std::time::Duration,
    ) -> Result<BackendResponse, AdapterError> {
        // Route to the physical model name.
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String(model.to_string()));
        }
        let url = format!("{}{}", endpoint.trim_end_matches('/'), api.path());
        let mut req = http.post(&url).json(&body).timeout(timeout);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }

        send_stream(req, cancel).await
    }

    /// Forward a raw multipart/form-data body to `{endpoint}{api.path()}`,
    /// rewriting the `model` form field to the physical model name. File
    /// parts, filenames and part content types are preserved.
    #[allow(clippy::too_many_arguments)]
    pub async fn forward_multipart_openai(
        http: &reqwest::Client,
        endpoint: &str,
        api_key: &str,
        api: ApiType,
        model: &str,
        body: &[u8],
        content_type: &str,
        cancel: CancellationToken,
        timeout: std::time::Duration,
    ) -> Result<BackendResponse, AdapterError> {
        let boundary = multer::parse_boundary(content_type)
            .map_err(|e| AdapterError::Protocol(format!("multipart: {e}")))?;
        let bytes = bytes::Bytes::copy_from_slice(body);
        let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(bytes) });
        let mut mp = multer::Multipart::new(stream, boundary);
        let mut form = reqwest::multipart::Form::new();
        while let Some(field) = mp
            .next_field()
            .await
            .map_err(|e| AdapterError::Protocol(format!("multipart: {e}")))?
        {
            let name = field.name().unwrap_or_default().to_string();
            if name == "model" {
                continue; // rewritten with the deployment's physical name below
            }
            let fname = field.file_name().map(str::to_string);
            let fct = field.content_type().map(|m| m.to_string());
            let data = field
                .bytes()
                .await
                .map_err(|e| AdapterError::Io(e.to_string()))?;
            let mut part = reqwest::multipart::Part::bytes(data.to_vec());
            if let Some(f) = fname {
                part = part.file_name(f);
            }
            if let Some(c) = fct {
                // mime_str consumes the part on failure — rebuild one from the
                // (cheaply cloned) data in that unreachable case.
                part = match part.mime_str(&c) {
                    Ok(p) => p,
                    Err(_) => reqwest::multipart::Part::bytes(data.to_vec()),
                };
            }
            form = form.part(name, part);
        }
        form = form.text("model", model.to_string());
        let url = format!("{}{}", endpoint.trim_end_matches('/'), api.path());
        let mut req = http.post(&url).multipart(form).timeout(timeout);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        send_stream(req, cancel).await
    }

    /// Send a fully-built request and return the raw byte stream. Shared by
    /// adapters with custom auth/URL schemes (Azure, Anthropic).
    pub async fn send_stream(
        req: reqwest::RequestBuilder,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        let send = req.send();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(AdapterError::Cancelled),
            r = send => r.map_err(|e| {
                if e.is_timeout() { AdapterError::Timeout }
                else if e.is_connect() { AdapterError::Connect(e.to_string()) }
                else { AdapterError::Io(e.to_string()) }
            })?,
        };

        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let is_sse = content_type.contains("text/event-stream");

        let cancel2 = cancel.clone();
        let stream = resp.bytes_stream().map(move |r| {
            if cancel2.is_cancelled() {
                return Err(AdapterError::Cancelled);
            }
            r.map_err(|e| AdapterError::Io(e.to_string()))
        });

        Ok(BackendResponse {
            status,
            is_event_stream: is_sse,
            stream: Box::pin(stream),
            content_type,
        })
    }

    /// GET JSON helper with timeout.
    pub async fn get_json(
        http: &reqwest::Client,
        url: &str,
        api_key: &str,
        timeout: std::time::Duration,
    ) -> Result<Value, AdapterError> {
        let mut req = http.get(url).timeout(timeout);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                AdapterError::Timeout
            } else {
                AdapterError::Connect(e.to_string())
            }
        })?;
        if !resp.status().is_success() {
            return Err(AdapterError::Upstream {
                status: resp.status().as_u16(),
                message: resp.status().to_string(),
            });
        }
        resp.json::<Value>()
            .await
            .map_err(|e| AdapterError::Protocol(e.to_string()))
    }

    /// Parse model ids from an OpenAI /v1/models response.
    pub fn parse_openai_models(v: &Value) -> Vec<String> {
        v.get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}
