//! Azure OpenAI adapter: OpenAI-compatible API but with a different URL
//! scheme and auth header.
//!
//!   {endpoint}/openai/deployments/{deployment}/chat/completions?api-version=X
//!   header: api-key: <key>          (not Authorization: Bearer)
//!
//! The LOOM deployment's `model_name` maps to the Azure *deployment name*.

use super::common;
use super::{AdapterError, BackendAdapter, BackendResponse, Health};
use async_trait::async_trait;
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct AzureAdapter {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl AzureAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }

    /// {endpoint}/openai/deployments/{dep}/{op}?api-version={v}
    fn deploy_url(&self, model: &str, op: &str) -> String {
        format!(
            "{}/openai/deployments/{}/{}?api-version={}",
            self.endpoint(),
            model,
            op,
            self.cfg.api_version
        )
    }

    fn op_path(api: ApiType) -> &'static str {
        match api {
            ApiType::Chat => "chat/completions",
            ApiType::Completions => "completions",
            ApiType::Embeddings => "embeddings",
            ApiType::Multi => "chat/completions",
        }
    }
}

#[async_trait]
impl BackendAdapter for AzureAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::AzureOpenAi
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        // Azure lists deployed models at /openai/models (resource account).
        let url = format!(
            "{}/openai/models?api-version={}",
            self.endpoint(),
            self.cfg.api_version
        );
        let req = self
            .http
            .get(&url)
            .header("api-key", &self.cfg.api_key)
            .timeout(Duration::from_secs(10));
        let resp = req
            .send()
            .await
            .map_err(|e| AdapterError::Connect(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(AdapterError::Upstream {
                status: resp.status().as_u16(),
                message: resp.status().to_string(),
            });
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| AdapterError::Protocol(e.to_string()))?;
        Ok(common::parse_openai_models(&v))
    }

    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError> {
        // Azure deployments are already-loaded; model name = deployment.
        // Embeddings probe: cheap 1-input request against the deployment.
        let mut caps = Capabilities {
            chat: true,
            completions: true,
            streaming: true,
            tools: true,
            json_mode: true,
            ..Default::default()
        };
        let req = self
            .http
            .post(self.deploy_url(model, "embeddings"))
            .header("api-key", &self.cfg.api_key)
            .json(&serde_json::json!({"input": "probe"}))
            .timeout(Duration::from_secs(5));
        if let Ok(r) = req.send().await {
            caps.embeddings = r.status().is_success();
        }
        Ok(caps)
    }

    async fn health(&self) -> Health {
        match self.list_models().await {
            Ok(_) => Health::Healthy,
            Err(AdapterError::Timeout) | Err(AdapterError::Connect(_)) => Health::Down,
            Err(_) => Health::Degraded,
        }
    }

    async fn warm_up(&self, model: &str) -> Result<(), AdapterError> {
        // Azure models are always loaded — a 1-token ping validates auth.
        let req = self
            .http
            .post(self.deploy_url(model, "chat/completions"))
            .header("api-key", &self.cfg.api_key)
            .json(&serde_json::json!({
                "messages": [{"role":"user","content":"hi"}],
                "max_tokens": 1
            }))
            .timeout(Duration::from_secs(30));
        match req.send().await {
            Ok(_) => Ok(()),
            Err(e) => Err(AdapterError::Connect(e.to_string())),
        }
    }

    async fn forward(
        &self,
        api: ApiType,
        model: &str,
        mut body: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, AdapterError> {
        // Deployment name goes in the URL; the body's `model` field is ignored
        // by Azure but kept for schema compatibility.
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String(model.to_string()));
        }
        let req = self
            .http
            .post(self.deploy_url(model, Self::op_path(api)))
            .header("api-key", &self.cfg.api_key)
            .json(&body)
            .timeout(Duration::from_secs(600));
        common::send_stream(req, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BackendConfig {
        BackendConfig {
            id: "b".into(),
            name: "az".into(),
            backend_type: BackendType::AzureOpenAi,
            endpoint: "https://acct.openai.azure.com".into(),
            api_key: "k".into(),
            deployments: vec![],
            enabled: true,
            remote: true,
            api_version: "2024-10-21".into(),
            health_interval_secs: 15,
        }
    }

    #[test]
    fn azure_deploy_url() {
        let a = AzureAdapter::new(cfg(), reqwest::Client::new());
        assert_eq!(
            a.deploy_url("gpt-4o-dep", "chat/completions"),
            "https://acct.openai.azure.com/openai/deployments/gpt-4o-dep/chat/completions?api-version=2024-10-21"
        );
    }
}
