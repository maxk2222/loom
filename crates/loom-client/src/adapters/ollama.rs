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
        let v = common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(10)).await?;
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
            // Vision: details.families contains "clip"/"mllama" etc.
            let families = info
                .get("details")
                .and_then(|d| d.get("families"))
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            for fam in families {
                if let Some(s) = fam.as_str() {
                    let s = s.to_lowercase();
                    if s.contains("clip") || s.contains("vision") || s.contains("mllama") {
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
        let v: Value = resp
            .json()
            .await
            .map_err(|e| AdapterError::Protocol(e.to_string()))?;
        if status != 200 {
            return Err(AdapterError::Upstream {
                status,
                message: v.get("error").and_then(|e| e.as_str()).unwrap_or("ollama error").to_string(),
            });
        }
        // { "embeddings": [[..],[..]] } → OpenAI list shape
        let embs = v.get("embeddings").and_then(|e| e.as_array()).cloned().unwrap_or_default();
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
