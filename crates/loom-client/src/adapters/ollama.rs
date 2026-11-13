//! Ollama adapter: uses Ollama's OpenAI-compatible endpoints for inference
//! (/v1/*) and Ollama-native endpoints (/api/tags, /api/show) for model
//! listing and capability inference.

use super::common;
use super::{AdapterError, BackendAdapter, BackendResponse, Health};
use async_trait::async_trait;
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct OllamaAdapter {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl OllamaAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }

    /// Native /api/show metadata for a model (context length, families).
    async fn show(&self, model: &str) -> Option<Value> {
        let url = format!("{}/api/show", self.endpoint());
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({"model": model}))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }
}

#[async_trait]
impl BackendAdapter for OllamaAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::Ollama
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        // Native listing is authoritative for Ollama.
        let url = format!("{}/api/tags", self.endpoint());
        let v =
            common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(10)).await?;
        let models = v
            .get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m.get("name")
                            .or_else(|| m.get("model"))
                            .and_then(|n| n.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    }

    /// Ollama: `/api/ps` lists models currently resident in memory.
    async fn loaded_models(&self) -> Option<Vec<String>> {
        let url = format!("{}/api/ps", self.endpoint());
        let v = common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5))
            .await
            .ok()?;
        let models = v
            .get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m.get("name")
                            .or_else(|| m.get("model"))
                            .and_then(|n| n.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(models)
    }

    /// Ollama gives every request its own context window — the deployment
    /// envelope is context_length × parallel slots. `/api/ps` reports the
    /// *effective* loaded context (env overrides like OLLAMA_CONTEXT_LENGTH
    /// only show up there); `/api/show` carries the model's default.
    async fn kv_budget_tokens(&self, model: &str, slots: u32) -> u64 {
        let mut ctx = 0u64;
        let url = format!("{}/api/ps", self.endpoint());
        if let Ok(v) =
            common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5)).await
        {
            ctx = v["models"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|m| m["name"].as_str() == Some(model) || m["model"].as_str() == Some(model))
                .and_then(|m| m["context_length"].as_u64())
                .unwrap_or(0);
        }
        if ctx == 0 {
            if let Some(info) = self.show(model).await {
                if let Some(mi) = info.get("model_info").and_then(|v| v.as_object()) {
                    ctx = mi
                        .iter()
                        .find(|(k, _)| k.ends_with("context_length"))
                        .and_then(|(_, v)| v.as_u64())
                        .unwrap_or(0);
                }
            }
        }
        ctx * u64::from(slots.max(1))
    }

    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError> {
        let mut caps = Capabilities {
            chat: true,
            completions: true,
            streaming: true,
            tools: true,
            json_mode: true,
            ..Default::default()
        };
        caps.modalities.insert("text".into());

        if let Some(info) = self.show(model).await {
            // Context length: model_info contains e.g. "llama.context_length": 8192
            if let Some(mi) = info.get("model_info").and_then(|v| v.as_object()) {
                for (k, v) in mi {
                    if k.ends_with("context_length") {
                        if let Some(n) = v.as_u64() {
                            caps.context_length = n as u32;
                        }
                    }
                    if k.ends_with("embedding_length") && v.as_u64().is_some() {
                        caps.embeddings = true;
                    }
                }
            }
            // Modern Ollama (>=0.9-ish) reports a top-level `capabilities`
            // array — authoritative for vision/tools/thinking/embedding.
            let declared: Vec<String> = info
                .get("capabilities")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();
            if declared.iter().any(|c| c == "vision") {
                caps.vision = true;
                caps.modalities.insert("image".into());
            }
            if declared.iter().any(|c| c == "embedding") {
                caps.embeddings = true;
            }
            // Older multimodal models expose a `projector_info` block.
            if info.get("projector_info").is_some() {
                caps.vision = true;
                caps.modalities.insert("image".into());
            }
            // Vision fallback: details.families contains "clip"/"mllama" etc.
            // Vision arch names (qwen3vl, moondream, minicpm-v) don't carry
            // those markers — the `vl`/`vision` name match covers them.
            let families = info
                .get("details")
                .and_then(|d| d.get("families"))
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            for fam in families {
                if let Some(s) = fam.as_str() {
                    let s = s.to_lowercase();
                    if s.contains("clip")
                        || s.contains("vision")
                        || s.contains("mllama")
                        || s.ends_with("vl")
                    {
                        caps.vision = true;
                        caps.modalities.insert("image".into());
                    }
                }
            }
        }

        // Embeddings: probe OpenAI-compatible endpoint (newer Ollama) or native.
        let url = format!("{}/api/embed", self.endpoint());
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({"model": model, "input": "probe"}))
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        if let Ok(r) = resp {
            if r.status().is_success() {
                caps.embeddings = true;
            }
        }
        Ok(caps)
    }

    async fn health(&self) -> Health {
        let url = format!("{}/api/tags", self.endpoint());
        match common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(5)).await {
            Ok(_) => Health::Healthy,
            Err(AdapterError::Timeout) | Err(AdapterError::Connect(_)) => Health::Down,
            Err(_) => Health::Degraded,
        }
    }

    /// Ollama: keep_alive:-1 loads the model into memory and pins it.
    async fn warm_up(&self, model: &str) -> Result<(), AdapterError> {
        self.keep_alive(model, -1, 120, "warm_up failed").await
    }

    /// Ollama: keep_alive:0 evicts the model from memory immediately.
    async fn unload(&self, model: &str) -> Result<(), AdapterError> {
        self.keep_alive(model, 0, 30, "unload failed").await
    }

    async fn forward(
        &self,
        api: ApiType,
        model: &str,
        mut body: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        // Ollama's /v1/embeddings exists on newer versions; fall back to native
        // /api/embed by translating the request/response shape.
        if api == ApiType::Embeddings {
            return self.forward_embeddings_native(model, body, cancel).await;
        }
        let _ = &mut body;
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
}

impl OllamaAdapter {
    /// keep_alive drive: /api/generate for chat models, /api/embed fallback for
    /// embedding-only models (they 400 on generate).
    async fn keep_alive(
        &self,
        model: &str,
        keep_alive: i64,
        timeout_secs: u64,
        err_msg: &'static str,
    ) -> Result<(), AdapterError> {
        let mut body = serde_json::json!({
            "model": model, "prompt": "", "keep_alive": keep_alive, "stream": false
        });
        let mut embed_fallback = false;
        for attempt in 0..2 {
            let url = format!(
                "{}/api/{}",
                self.endpoint(),
                if embed_fallback { "embed" } else { "generate" }
            );
            let mut req = self
                .http
                .post(&url)
                .json(&body)
                .timeout(std::time::Duration::from_secs(timeout_secs));
            if !self.cfg.api_key.is_empty() {
                req = req.bearer_auth(&self.cfg.api_key);
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) if attempt == 0 && r.status().as_u16() == 400 => {
                    embed_fallback = true;
                    body = serde_json::json!({
                        "model": model, "input": "w", "keep_alive": keep_alive, "stream": false
                    });
                    continue;
                }
                Ok(r) => {
                    return Err(AdapterError::Upstream {
                        status: r.status().as_u16(),
                        message: err_msg.into(),
                    })
                }
                Err(e) => return Err(AdapterError::Connect(e.to_string())),
            }
        }
        unreachable!()
    }

    /// Native /api/embed translation → OpenAI embeddings response shape.
    async fn forward_embeddings_native(
        &self,
        model: &str,
        body: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        use futures_util::stream;
        let input = body.get("input").cloned().unwrap_or(Value::Null);
        let url = format!("{}/api/embed", self.endpoint());
        let req = self
            .http
            .post(&url)
            .json(&serde_json::json!({"model": model, "input": input}))
            .timeout(Duration::from_secs(120))
            .send();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(AdapterError::Cancelled),
            r = req => r.map_err(|e| AdapterError::Connect(e.to_string()))?,
        };
        let status = resp.status().as_u16();
        // The body read is a second await — a large embedding response on a
        // slow socket would otherwise ignore cancellation until the body
        // completes.
        let v: Value = tokio::select! {
            _ = cancel.cancelled() => return Err(AdapterError::Cancelled),
            r = resp.json() => r.map_err(|e| AdapterError::Protocol(e.to_string()))?,
        };
        if status != 200 {
            return Err(AdapterError::Upstream {
                status,
                message: v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("ollama error")
                    .to_string(),
            });
        }
        // { "embeddings": [[..],[..]] } → OpenAI list shape
        let embs = v
            .get("embeddings")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();
        let data: Vec<Value> = embs
            .into_iter()
            .enumerate()
            .map(|(i, e)| serde_json::json!({"object":"embedding","index":i,"embedding":e}))
            .collect();
        let out = serde_json::json!({
            "object": "list",
            "data": data,
            "model": model,
            "usage": {"prompt_tokens":0,"total_tokens":0},
        });
        let bytes = bytes::Bytes::from(serde_json::to_vec(&out).unwrap_or_default());
        Ok(BackendResponse {
            status: 200,
            is_event_stream: false,
            stream: Box::pin(stream::once(async move { Ok(bytes) })),
            content_type: "application/json".into(),
        })
    }
}
