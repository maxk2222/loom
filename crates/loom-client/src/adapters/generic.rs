//! Generic OpenAI-compatible adapter: works with any server implementing
//! /v1/chat/completions (+ optionally completions/embeddings/models).
//! Covers SGLang, llama.cpp server, TGI-compatibles, etc.

use super::common;
use super::{AdapterError, BackendAdapter, BackendResponse, Health};
use async_trait::async_trait;
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct GenericAdapter {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl GenericAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }

    /// LM Studio's own model registry (`/api/v1/models`) — returns the model
    /// entry whose key/display name/loaded id matches. `Ok(None)` when the
    /// endpoint is absent (plain OpenAI-compatible servers don't have it);
    /// transient errors propagate so the caller retries instead of caching
    /// conservative defaults forever.
    async fn lmstudio_model_info(
        &self,
        model: &str,
    ) -> Result<Option<serde_json::Value>, AdapterError> {
        let url = format!("{}/api/v1/models", self.endpoint());
        let v = match common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5))
            .await
        {
            Ok(v) => v,
            // 4xx = endpoint absent → not an LM Studio server.
            Err(AdapterError::Upstream { status, .. }) if (400..500).contains(&status) => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let Some(models) = v["models"].as_array() else {
            // Endpoint answered but isn't LM Studio's registry shape.
            return Ok(None);
        };
        Ok(models
            .iter()
            .find(|m| {
                m["key"].as_str() == Some(model)
                    || m["display_name"].as_str() == Some(model)
                    || m["loaded_instances"]
                        .as_array()
                        .is_some_and(|li| li.iter().any(|i| i["id"].as_str() == Some(model)))
            })
            .cloned())
    }
}

#[async_trait]
impl BackendAdapter for GenericAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::GenericOpenAi
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        let url = format!("{}/v1/models", self.endpoint());
        let v =
            common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(10)).await?;
        Ok(common::parse_openai_models(&v))
    }

    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError> {
        // Generic probe: model exists in listing → assume chat + streaming.
        // Conservative defaults; admins can override per deployment.
        // Propagate list errors — a backend that can't list models right now
        // must be re-probed on the next pass, not cached with defaults.
        let models = self.list_models().await?;
        let mut caps = Capabilities {
            streaming: true,
            // OpenAI-compatible chat APIs accept tools/json_mode body params —
            // worst case the backend ignores or errors, which is a better UX
            // than wrongly blocking. Vision stays opt-in (input-format-specific).
            tools: true,
            json_mode: true,
            ..Default::default()
        };
        if models.iter().any(|m| m == model) || models.is_empty() {
            caps.chat = true;
            caps.completions = true;
        }
        // LM Studio exposes a richer model registry at /api/v1/models with
        // real per-model capabilities — use it when present (harmless 404
        // on other OpenAI-compatible servers).
        if let Some(info) = self.lmstudio_model_info(model).await? {
            if info["capabilities"]["vision"].as_bool() == Some(true) {
                caps.vision = true;
                caps.modalities.insert("image".into());
            }
            caps.tools |= info["capabilities"]["trained_for_tool_use"].as_bool() == Some(true);
            if let Some(n) = info["max_context_length"].as_u64() {
                caps.context_length = n as u32;
            }
            match info["type"].as_str() {
                // LM Studio reports "embedding" (singular); keep "embeddings"
                // for other registries' variants.
                Some("embedding") | Some("embeddings") => {
                    caps.embeddings = true;
                    // Pure embedding models appear in /v1/models too, but
                    // cannot serve chat/completions.
                    caps.chat = false;
                    caps.completions = false;
                }
                Some("vlm") => {
                    caps.vision = true;
                    caps.modalities.insert("image".into());
                }
                _ => {}
            }
        }
        // Embeddings: try a cheap request to detect support. Upgrade only —
        // a probe failure (cold load, transient 500) must not erase a
        // capability the registry already declared.
        let url = format!("{}/v1/embeddings", self.endpoint());
        let mut req = self
            .http
            .post(&url)
            .json(&serde_json::json!({"model": model, "input": "probe"}))
            .timeout(Duration::from_secs(5));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        if let Ok(r) = req.send().await {
            caps.embeddings |= r.status().is_success();
        }
        Ok(caps)
    }

    /// Loaded instances from LM Studio's registry — `Some(set)` when the
    /// endpoint exists (empty set = "nothing resident"), `None` elsewhere
    /// (permissive: treat as unknown-loadable).
    async fn loaded_models(&self) -> Option<Vec<String>> {
        let url = format!("{}/api/v1/models", self.endpoint());
        let v = common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5))
            .await
            .ok()?;
        let mut out = Vec::new();
        for m in v["models"].as_array()? {
            for i in m["loaded_instances"].as_array().into_iter().flatten() {
                if let Some(id) = i["id"].as_str() {
                    out.push(id.to_string());
                }
            }
        }
        Some(out)
    }

    /// Like `loaded_models` but embedding-only registry entries stay out:
    /// an `auto` deployment must bind a chat-serving model.
    async fn bindable_models(&self) -> Option<Vec<String>> {
        let url = format!("{}/api/v1/models", self.endpoint());
        let v = common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5))
            .await
            .ok()?;
        let mut out = Vec::new();
        for m in v["models"].as_array()? {
            if m["type"].as_str() == Some("embedding") {
                continue;
            }
            for i in m["loaded_instances"].as_array().into_iter().flatten() {
                if let Some(id) = i["id"].as_str() {
                    out.push(id.to_string());
                }
            }
        }
        Some(out)
    }

    /// Installed non-embedding models — `/v1/models` ordering is unstable
    /// (LM Studio puts resident models first), so eligibility must come
    /// from the typed registry, not list position.
    async fn bindable_installed(&self) -> Option<Vec<String>> {
        let url = format!("{}/api/v1/models", self.endpoint());
        let v = common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5))
            .await
            .ok()?;
        Some(
            v["models"]
                .as_array()?
                .iter()
                .filter(|m| m["type"].as_str() != Some("embedding"))
                .filter_map(|m| m["key"].as_str().map(String::from))
                .collect(),
        )
    }

    /// LM Studio's registry reports `max_context_length` per model; each
    /// request gets its own context window → ctx × slots. Plain
    /// OpenAI-compatible servers expose no capacity signal → 0.
    async fn kv_budget_tokens(&self, model: &str, slots: u32) -> u64 {
        let Ok(Some(info)) = self.lmstudio_model_info(model).await else {
            return 0;
        };
        info["max_context_length"]
            .as_u64()
            .map(|ctx| ctx * u64::from(slots.max(1)))
            .unwrap_or(0)
    }

    async fn health(&self) -> Health {
        let url = format!("{}/v1/models", self.endpoint());
        match common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5)).await {
            Ok(_) => Health::Healthy,
            Err(AdapterError::Timeout) | Err(AdapterError::Connect(_)) => Health::Down,
            Err(_) => Health::Degraded,
        }
    }

    /// Generic OpenAI: a minimal request triggers JIT model load. The
    /// endpoint must match the model type — a chat request against an
    /// embedding-only model is rejected and never loads it (observed on
    /// LM Studio: endless warm loop, deployment stuck draining).
    async fn warm_up(&self, model: &str) -> Result<(), AdapterError> {
        let kind = self
            .lmstudio_model_info(model)
            .await
            .ok()
            .flatten()
            .and_then(|i| i["type"].as_str().map(String::from));
        let (path, body) = match kind.as_deref() {
            Some("embedding") | Some("embeddings") => (
                "/v1/embeddings",
                serde_json::json!({"model": model, "input": "warmup"}),
            ),
            _ => (
                "/v1/chat/completions",
                serde_json::json!({
                    "model": model,
                    "messages": [{"role":"user","content":"hi"}],
                    "max_tokens": 1,
                    "stream": false
                }),
            ),
        };
        let url = format!("{}{}", self.endpoint(), path);
        let mut req = self
            .http
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(120));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        match req.send().await {
            // 4xx = backend reached, JIT load triggered but the request shape
            // was rejected — the model is still loading. 5xx is a real fault.
            Ok(r) if r.status().is_server_error() => Err(AdapterError::Upstream {
                status: r.status().as_u16(),
                message: format!("warm_up failed: {}", r.status()),
            }),
            Ok(r) if r.status().is_success() => {
                // A wedged backend can answer 200 with an empty body or empty
                // choices mid load-transition — that is NOT warm. Verify the
                // response carries a real completion before reporting ready.
                let status = r.status().as_u16();
                let v: Value = r
                    .json()
                    .await
                    .map_err(|e| AdapterError::Protocol(e.to_string()))?;
                let produced = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .map(|c| {
                        c.iter().any(|ch| {
                            ch["message"]["content"]
                                .as_str()
                                .is_some_and(|s| !s.trim().is_empty())
                                || ch["text"].as_str().is_some_and(|s| !s.trim().is_empty())
                        })
                    })
                    .unwrap_or_else(|| {
                        // Embeddings path: `data` non-empty = real output.
                        v.get("data")
                            .and_then(|d| d.as_array())
                            .is_some_and(|d| !d.is_empty())
                    });
                if produced {
                    Ok(())
                } else {
                    Err(AdapterError::Upstream {
                        status,
                        message: "warm_up: empty completion — model still loading".into(),
                    })
                }
            }
            Ok(_) => Ok(()),
            Err(e) => Err(AdapterError::Connect(e.to_string())),
        }
    }

    /// LM Studio ≥0.4 exposes `POST /api/v1/models/unload {instance_id}`.
    /// The loaded instance id usually equals the model key, but custom
    /// instances can differ — resolve it through the registry first. A model
    /// that isn't loaded is already in the desired state; backends without
    /// the endpoint answer 4xx, which the caller treats as unsupported.
    async fn unload(&self, model: &str) -> Result<(), AdapterError> {
        let Some(info) = self.lmstudio_model_info(model).await? else {
            return Err(AdapterError::Protocol(
                "unload not supported by this backend".into(),
            ));
        };
        let Some(instance_id) = info["loaded_instances"].as_array().and_then(|li| {
            li.iter()
                .find(|i| i["id"].as_str() == Some(model))
                .or_else(|| li.first())
                .and_then(|i| i["id"].as_str().map(String::from))
        }) else {
            return Ok(());
        };
        let url = format!("{}/api/v1/models/unload", self.endpoint());
        let mut req = self
            .http
            .post(&url)
            .json(&serde_json::json!({"instance_id": instance_id}))
            .timeout(Duration::from_secs(30));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(AdapterError::Upstream {
                status: r.status().as_u16(),
                message: format!("unload failed: {}", r.status()),
            }),
            Err(e) => Err(AdapterError::Connect(e.to_string())),
        }
    }

    async fn forward(
        &self,
        api: ApiType,
        model: &str,
        body: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        common::forward_openai(
            &self.http,
            self.endpoint(),
            &self.cfg.api_key,
            api,
            model,
            body,
            cancel,
            Duration::from_secs(600),
        )
        .await
    }

    async fn forward_multipart(
        &self,
        api: ApiType,
        model: &str,
        body: &[u8],
        content_type: &str,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        common::forward_multipart_openai(
            &self.http,
            self.endpoint(),
            &self.cfg.api_key,
            api,
            model,
            body,
            content_type,
            cancel,
            Duration::from_secs(600),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use serde_json::json;

    fn cfg(endpoint: String) -> BackendConfig {
        BackendConfig {
            id: "b".into(),
            name: "g".into(),
            backend_type: BackendType::GenericOpenAi,
            endpoint,
            api_key: String::new(),
            deployments: vec![],
            enabled: true,
            remote: false,
            api_version: String::new(),
            health_interval_secs: 15,
        }
    }

    /// Mini LM Studio lookalike: /v1/models lists ids, /api/v1/models
    /// answers with the rich registry shape (or a configurable failure).
    async fn spawn_lmstudio(model: &str, registry_status: u16) -> String {
        let model = model.to_string();
        let registry = move || {
            let model = model.clone();
            async move {
                if registry_status != 200 {
                    return (
                        StatusCode::from_u16(registry_status).unwrap(),
                        Json(json!({})),
                    )
                        .into_response();
                }
                Json(json!({
                    "models": [{
                        "key": model,
                        "type": "vlm",
                        "capabilities": {"vision": true, "trained_for_tool_use": true},
                        "max_context_length": 262144,
                        "loaded_instances": [{"id": model}],
                    }]
                }))
                .into_response()
            }
        };
        let m2 = {
            let model = "text-embed".to_string();
            move || {
                let model = model.clone();
                async move {
                    Json(json!({"object":"list","data":[
                        {"id":model,"object":"model"},
                        {"id":"text-embed","object":"model"}
                    ]}))
                }
            }
        };
        let app = Router::new()
            .route("/v1/models", get(m2))
            .route("/api/v1/models", get(registry))
            .route("/v1/embeddings", post(|| async { StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn lmstudio_unload_posts_resolved_instance_id() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let registry = || async {
            Json(json!({
                "models": [{
                    "key": "m",
                    "type": "llm",
                    "loaded_instances": [{"id": "inst-42"}],
                }]
            }))
        };
        let app = Router::new().route("/api/v1/models", get(registry)).route(
            "/api/v1/models/unload",
            post(move |Json(b): Json<serde_json::Value>| {
                let h = h2.clone();
                async move {
                    assert_eq!(b["instance_id"], "inst-42");
                    h.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"instance_id": "inst-42"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let a = GenericAdapter::new(cfg(format!("http://{addr}")), reqwest::Client::new());
        a.unload("m").await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lmstudio_unload_not_loaded_is_noop() {
        let app = Router::new().route(
            "/api/v1/models",
            get(|| async { Json(json!({"models": [{"key": "m", "loaded_instances": []}]})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let a = GenericAdapter::new(cfg(format!("http://{addr}")), reqwest::Client::new());
        assert!(a.unload("m").await.is_ok());
    }

    #[tokio::test]
    async fn unload_non_lmstudio_is_unsupported() {
        // Plain OpenAI server: /api/v1/models is a 404 → unsupported, not a
        // silent success that would pretend memory was freed.
        let app = Router::new().route("/api/v1/models", get(|| async { StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let a = GenericAdapter::new(cfg(format!("http://{addr}")), reqwest::Client::new());
        assert!(a.unload("m").await.is_err());
    }

    #[tokio::test]
    async fn lmstudio_probe_reads_vision_and_context() {
        let url = spawn_lmstudio("m-vlm", 200).await;
        let a = GenericAdapter::new(cfg(url), reqwest::Client::new());
        let caps = a.probe_capabilities("m-vlm").await.unwrap();
        assert!(caps.vision);
        assert!(caps.tools);
        assert!(caps.modalities.contains("image"));
        assert_eq!(caps.context_length, 262144);
        // embeddings probe POSTed and got 404 → stays false.
        assert!(!caps.embeddings);
    }

    #[tokio::test]
    async fn lmstudio_probe_absent_endpoint_is_conservative_not_error() {
        // Plain OpenAI-compatible server: no /api/v1/models (404) → Ok(None)
        // → conservative caps, no retry needed.
        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async {
                    Json(json!({"object":"list","data":[{"id":"m","object":"model"}]}))
                }),
            )
            .route("/v1/embeddings", post(|| async { StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let a = GenericAdapter::new(cfg(format!("http://{addr}")), reqwest::Client::new());
        let caps = a.probe_capabilities("m").await.unwrap();
        assert!(caps.chat);
        assert!(!caps.vision);
        assert_eq!(caps.context_length, 0);
    }

    #[tokio::test]
    async fn lmstudio_probe_transient_error_propagates_for_retry() {
        // 5xx on the registry → probe fails → caps stay unset → next health
        // pass retries (previously this cached conservative caps forever).
        let url = spawn_lmstudio("m", 500).await;
        let a = GenericAdapter::new(cfg(url), reqwest::Client::new());
        assert!(a.probe_capabilities("m").await.is_err());
    }

    #[tokio::test]
    async fn lmstudio_loaded_models_lists_resident_instances() {
        let url = spawn_lmstudio("m-vlm", 200).await;
        let a = GenericAdapter::new(cfg(url), reqwest::Client::new());
        let loaded = a.loaded_models().await.unwrap();
        assert_eq!(loaded, vec!["m-vlm".to_string()]);
    }

    /// warm_up must hit the endpoint matching the model type: an
    /// embedding-type model warmed via /v1/chat/completions is rejected by
    /// LM Studio and never loads (endless re-warm loop).
    #[tokio::test]
    async fn warm_up_uses_embeddings_endpoint_for_embed_models() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let embed_hits = {
            let h = h.clone();
            move |_: Json<serde_json::Value>| {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"data":[{"embedding":[0.1]}]}))
                }
            }
        };
        let chat_hits = {
            let h = hits.clone();
            move |_: Json<serde_json::Value>| {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::BAD_REQUEST, Json(json!({"error":"no chat"}))).into_response()
                }
            }
        };
        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async {
                    Json(json!({"object":"list","data":[{"id":"m-emb","object":"model"}]}))
                }),
            )
            .route(
                "/api/v1/models",
                get(|| async {
                    Json(json!({"models":[{"key":"m-emb","type":"embeddings",
                        "capabilities":{},"loaded_instances":[]}]}))
                }),
            )
            .route("/v1/embeddings", post(embed_hits))
            .route("/v1/chat/completions", post(chat_hits));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let a = GenericAdapter::new(cfg(format!("http://{addr}")), reqwest::Client::new());
        a.warm_up("m-emb").await.unwrap();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "expected exactly one warm call"
        );
    }
}
