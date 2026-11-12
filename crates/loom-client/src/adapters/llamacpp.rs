//! llama.cpp server adapter: OpenAI-compatible (`/v1/*`) plus the native
//! model-router endpoints introduced with `llama-server --models-max`:
//!   POST /models/load   {"model": "name"}
//!   POST /models/unload {"model": "name"}
//!   GET  /health
//! This gives llama.cpp true auto-load parity with Ollama.

use super::generic::GenericAdapter;
use super::{AdapterError, BackendAdapter, BackendResponse, Health};
use async_trait::async_trait;
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct LlamaCppAdapter {
    generic: GenericAdapter,
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl LlamaCppAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self {
            generic: GenericAdapter::new(cfg.clone(), http.clone()),
            cfg,
            http,
        }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }
}

#[async_trait]
impl BackendAdapter for LlamaCppAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::LlamaCpp
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        self.generic.list_models().await
    }

    /// llama.cpp (router mode) lists only loaded models via /v1/models.
    async fn loaded_models(&self) -> Option<Vec<String>> {
        self.generic.list_models().await.ok()
    }

    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError> {
        self.generic.probe_capabilities(model).await
    }

    /// llama.cpp has one shared context pool: `/props` reports the
    /// server-wide `n_ctx` (default_generation_settings) — the pool itself,
    /// not per-slot.
    async fn kv_budget_tokens(&self, _model: &str, _slots: u32) -> u64 {
        let url = format!("{}/props", self.endpoint());
        let mut req = self.http.get(&url).timeout(Duration::from_secs(5));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        let Ok(resp) = req.send().await else {
            return 0;
        };
        if !resp.status().is_success() {
            return 0;
        }
        let v: Value = resp.json().await.unwrap_or_default();
        v["default_generation_settings"]["n_ctx"]
            .as_u64()
            .unwrap_or(0)
    }

    async fn health(&self) -> Health {
        // Native /health endpoint is cheap and unauthenticated.
        let req = self
            .http
            .get(format!("{}/health", self.endpoint()))
            .timeout(Duration::from_secs(5));
        match req.send().await {
            Ok(r) if r.status().is_success() => Health::Healthy,
            Ok(r) if r.status().as_u16() == 503 => Health::Degraded, // loading
            Ok(_) => Health::Degraded,
            Err(_) => self.generic.health().await, // older servers lack /health
        }
    }

    async fn warm_up(&self, model: &str) -> Result<(), AdapterError> {
        // Native router-mode load; fall back to a 1-token chat (JIT trigger)
        // when the endpoint is absent (single-model servers, older builds).
        let req = self
            .http
            .post(format!("{}/models/load", self.endpoint()))
            .json(&serde_json::json!({"model": model}))
            .timeout(Duration::from_secs(300));
        match req.send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            _ => self.generic.warm_up(model).await,
        }
    }

    /// Native router-mode unload (llama-server --models-max).
    async fn unload(&self, model: &str) -> Result<(), AdapterError> {
        let req = self
            .http
            .post(format!("{}/models/unload", self.endpoint()))
            .json(&serde_json::json!({"model": model}))
            .timeout(Duration::from_secs(30));
        match req.send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(AdapterError::Upstream {
                status: r.status().as_u16(),
                message: "unload failed".into(),
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
        self.generic.forward(api, model, body, cancel).await
    }
}
