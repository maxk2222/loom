//! vLLM adapter: OpenAI-compatible surface plus vLLM-specific health endpoint
//! (/health) and capability heuristics. Note: vLLM's own API key does not
//! protect all endpoints — the client isolates the backend port anyway.

use super::common;
use super::{AdapterError, BackendAdapter, BackendResponse, Health};
use async_trait::async_trait;
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct VllmAdapter {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl VllmAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }
}

#[async_trait]
impl BackendAdapter for VllmAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::Vllm
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        let url = format!("{}/v1/models", self.endpoint());
        let v = common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(10)).await?;
        Ok(common::parse_openai_models(&v))
    }

    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError> {
        let models = self.list_models().await.unwrap_or_default();
        let mut caps = Capabilities {
            chat: true,
            completions: true,
            streaming: true,
            tools: true, // vLLM supports tool calling for models that advertise it
            json_mode: true, // guided decoding
            ..Default::default()
        };
        caps.modalities.insert("text".into());

        // Vision detection: model name heuristics + /v1/models metadata if any.
        let lower = model.to_lowercase();
        if lower.contains("vl") || lower.contains("vision") || lower.contains("llava")
            || lower.contains("qwen2-vl") || lower.contains("pixtral")
        {
            caps.vision = true;
            caps.modalities.insert("image".into());
        }

        // Embeddings: probe endpoint; many vLLM builds also serve pooling models.
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
            caps.embeddings = r.status().is_success();
        }
        let _ = models;
        Ok(caps)
    }

    async fn health(&self) -> Health {
        // vLLM exposes /health (200 when serving).
        let url = format!("{}/health", self.endpoint());
        let mut req = self.http.get(&url).timeout(Duration::from_secs(5));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => Health::Healthy,
            Ok(_) => Health::Degraded,
            Err(e) if e.is_connect() || e.is_timeout() => Health::Down,
            Err(_) => Health::Unknown,
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
}
