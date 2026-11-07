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
        let models = self.list_models().await.unwrap_or_default();
        let mut caps = Capabilities {
            streaming: true,
            ..Default::default()
        };
        if models.iter().any(|m| m == model) || models.is_empty() {
            caps.chat = true;
            caps.completions = true;
        }
        // Embeddings: try a cheap request to detect support.
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
        Ok(caps)
    }

    async fn health(&self) -> Health {
        let url = format!("{}/v1/models", self.endpoint());
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
