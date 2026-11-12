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

/// Extract the effective KV token budget from a prometheus /metrics body:
/// `kv_cache_size_tokens` label wins (exact capacity for both pure-attention
/// and hybrid models), `num_gpu_blocks × block_size` is the legacy fallback.
fn kv_tokens_from_cache_config(body: &str) -> Option<u64> {
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("vllm:cache_config_info{") else {
            continue;
        };
        let label = |key: &str| -> Option<u64> {
            let marker = format!("{key}=\"");
            let start = rest.find(&marker)? + marker.len();
            let end = rest[start..].find('"')? + start;
            rest[start..end].parse().ok()
        };
        if let Some(n) = label("kv_cache_size_tokens") {
            if n > 0 {
                return Some(n);
            }
        }
        if let (Some(blocks), Some(size)) = (label("num_gpu_blocks"), label("block_size")) {
            return Some(blocks * size);
        }
    }
    None
}

impl VllmAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }

    /// vLLM's `/get_server_info` (newer builds) returns the launch config —
    /// `limit_mm_per_prompt` / mm-processor kwargs name the accepted input
    /// types ("image", "video", "audio"). Missing endpoint → empty set.
    async fn server_info_mm(&self) -> std::collections::HashSet<String> {
        let url = format!("{}/get_server_info", self.endpoint());
        let mut req = self.http.get(&url).timeout(Duration::from_secs(5));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        let Ok(resp) = req.send().await else {
            return Default::default();
        };
        if !resp.status().is_success() {
            return Default::default();
        }
        let v: Value = resp.json().await.unwrap_or_default();
        let mut out = std::collections::HashSet::new();
        collect_mm_types(&v, &mut out);
        out
    }

    /// `vllm:cache_config_info` info metric carries the physical KV pool
    /// geometry. Prefer its `kv_cache_size_tokens` label — the canonical
    /// capacity vLLM itself logs ("GPU KV cache size: N tokens"). Only fall
    /// back to num_gpu_blocks × block_size when the label is absent:
    /// hybrid models (mamba/GDN, e.g. Qwen3-Next) resolve block_size to the
    /// mamba page size and the product overshoots massively (observed:
    /// 5319 × 3008 ≈ 16M vs the real 880k on qwen3.8-flash-next).
    async fn metrics_kv_tokens(&self) -> Option<u64> {
        let url = format!("{}/metrics", self.endpoint());
        let mut req = self.http.get(&url).timeout(Duration::from_secs(5));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        let body = req.send().await.ok()?.text().await.ok()?;
        kv_tokens_from_cache_config(&body)
    }

    /// `/get_server_info` `max_model_len` — a conservative floor (admits at
    /// least one full-context request) when metrics can't be measured.
    async fn server_info_max_model_len(&self) -> u64 {
        let url = format!("{}/get_server_info", self.endpoint());
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
        fn find(v: &Value) -> Option<u64> {
            match v {
                Value::Object(m) => {
                    if let Some(n) = m.get("max_model_len").and_then(|n| n.as_u64()) {
                        return Some(n);
                    }
                    m.values().find_map(find)
                }
                Value::Array(a) => a.iter().find_map(find),
                _ => None,
            }
        }
        find(&v).unwrap_or(0)
    }

    /// Does the served model accept `kind` ("image"/"video") input parts?
    /// A real request proves support independent of model naming. Any
    /// transport/timeout error → treated as unsupported (conservative:
    /// admins can still force it via capability overrides).
    async fn mm_part_accepted(&self, model: &str, kind: &str) -> bool {
        let part = match kind {
            "image" => serde_json::json!({
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="}
            }),
            // Tiny dummy payload: a model that understands video_url parts
            // parses the block and fails on the media itself — that still
            // proves video input is plumbed through. A text-only model
            // rejects the part type outright.
            "video" => serde_json::json!({
                "type": "video_url",
                "video_url": {"url": "data:video/mp4;base64,AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXYx"}
            }),
            _ => return false,
        };
        let url = format!("{}/v1/chat/completions", self.endpoint());
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"},
                part
            ]}],
        });
        let mut req = self
            .http
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(20));
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        let Ok(resp) = req.send().await else {
            return false;
        };
        if resp.status().is_success() {
            return true;
        }
        // A valid embedded 1-px PNG has no excuse to fail except a missing
        // modality — treat every error as unsupported.
        if kind == "image" {
            return false;
        }
        // For the deliberately-broken video payload only the *kind* of error
        // is informative: a media decode/fetch failure means the part type
        // parsed fine (input accepted), while a rejection that names the
        // missing modality is conclusive. Anything else → unsupported.
        let err = resp.text().await.unwrap_or_default().to_lowercase();
        let conclusive_reject = err.contains("not support")
            || err.contains("multimodal")
            || err.contains("text-only")
            || err.contains("unexpected content");
        if conclusive_reject {
            return false;
        }
        err.contains("video")
            || err.contains("fetch")
            || err.contains("decode")
            || err.contains("load")
            || err.contains("media")
    }
}

#[async_trait]
impl BackendAdapter for VllmAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::Vllm
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        let url = format!("{}/v1/models", self.endpoint());
        let v =
            common::get_json(&self.http, &url, &self.cfg.api_key, Duration::from_secs(10)).await?;
        Ok(common::parse_openai_models(&v))
    }

    /// vLLM serves only resident models — the listing IS the loaded set.
    async fn loaded_models(&self) -> Option<Vec<String>> {
        self.list_models().await.ok()
    }

    async fn probe_capabilities(&self, model: &str) -> Result<Capabilities, AdapterError> {
        let models = self.list_models().await.unwrap_or_default();
        let mut caps = Capabilities {
            chat: true,
            completions: true,
            streaming: true,
            tools: true,     // vLLM supports tool calling for models that advertise it
            json_mode: true, // guided decoding
            ..Default::default()
        };
        caps.modalities.insert("text".into());

        // Vision detection, strongest signal first:
        //  1. /get_server_info — vLLM exposes its launch config; the
        //     multimodal limits ("limit_mm_per_prompt": {"image":n,"video":n})
        //     say exactly which input types the model accepts.
        //  2. Model name heuristics (vl/vision/llava/qwen2-vl/pixtral).
        //  3. Functional probe — a real 1-px image (and a video part) through
        //     chat/completions; vLLM rejects unsupported modalities with a
        //     clear error, success proves the path works regardless of name.
        let mut mm = self.server_info_mm().await;
        let lower = model.to_lowercase();
        if !mm.contains("image")
            && (lower.contains("vl")
                || lower.contains("vision")
                || lower.contains("llava")
                || lower.contains("qwen2-vl")
                || lower.contains("pixtral"))
        {
            mm.insert("image".into());
        }
        if !mm.contains("image") && self.mm_part_accepted(model, "image").await {
            mm.insert("image".into());
        }
        if mm.contains("image") {
            caps.vision = true;
            caps.modalities.insert("image".into());
            // Video only makes sense on an mm model — and the functional
            // probe costs a real inference round-trip.
            if !mm.contains("video") && self.mm_part_accepted(model, "video").await {
                mm.insert("video".into());
            }
        }
        if mm.contains("video") {
            caps.video_input = true;
            caps.modalities.insert("video".into());
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

    /// vLLM has one shared KV pool (the GPU blocks) — the budget is the
    /// pool size, independent of slot count.
    async fn kv_budget_tokens(&self, _model: &str, _slots: u32) -> u64 {
        if let Some(n) = self.metrics_kv_tokens().await {
            return n;
        }
        self.server_info_max_model_len().await
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

    // vLLM ≥0.5 serves /v1/audio/transcriptions for whisper-class models.
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

/// Walk `/get_server_info` JSON for multimodal limit/kwarg dicts and collect
/// the input types they name ("image", "video", "audio", ...). vLLM exposes
/// the relevant config under different keys across versions
/// (`limit_mm_per_prompt`, `media_io_kwargs`, `mm_processor_kwargs` — nested
/// in `vllm_config`/`model_config` on newer builds). Only objects inside a
/// key whose name marks them as mm-related are scanned — a bare
/// `{"image": "container:latest"}` elsewhere must not trip this.
fn collect_mm_types(v: &Value, out: &mut std::collections::HashSet<String>) {
    const MM_KEYS: [&str; 4] = ["image", "images", "video", "videos"];
    const MM_SCOPE: [&str; 4] = ["limit_mm", "media_io", "mm_processor", "multimodal"];
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let lk = k.to_lowercase();
                if MM_SCOPE.iter().any(|s| lk.contains(s)) {
                    if let Value::Object(inner) = child {
                        for key in MM_KEYS {
                            if inner.contains_key(key) {
                                out.insert(key.trim_end_matches('s').to_string());
                            }
                        }
                    }
                    // still recurse — deeper nesting happens across versions
                }
                collect_mm_types(child, out);
            }
        }
        Value::Array(a) => {
            for child in a {
                collect_mm_types(child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::kv_tokens_from_cache_config;

    // Real metric line from vllm serve qwen3.8-flash-next (hybrid GDN/mamba):
    // block_size resolves to the 3008-token mamba page, so blocks×size
    // (5319×3008≈16M) overshoots the true KV pool ~18x.
    const HYBRID: &str = r#"vllm:cache_config_info{block_size="3008",kv_cache_size_tokens="880267",num_gpu_blocks="5319",engine="0"} 1.0"#;
    // Pure-attention model: canonical label equals blocks×size.
    const DENSE: &str = r#"vllm:cache_config_info{block_size="16",kv_cache_size_tokens="1048576",num_gpu_blocks="65536",engine="0"} 1.0"#;
    // Older vLLM without the canonical label — legacy product still works.
    const LEGACY: &str =
        r#"vllm:cache_config_info{block_size="16",num_gpu_blocks="4096",engine="0"} 1.0"#;

    #[test]
    fn kv_budget_prefers_canonical_label_for_hybrid() {
        assert_eq!(kv_tokens_from_cache_config(HYBRID), Some(880267));
    }

    #[test]
    fn kv_budget_dense_model() {
        assert_eq!(kv_tokens_from_cache_config(DENSE), Some(1048576));
    }

    #[test]
    fn kv_budget_falls_back_to_blocks_times_size() {
        assert_eq!(kv_tokens_from_cache_config(LEGACY), Some(4096 * 16));
    }

    #[test]
    fn kv_budget_zero_label_falls_back() {
        let body = r#"vllm:cache_config_info{block_size="16",kv_cache_size_tokens="0",num_gpu_blocks="100",engine="0"} 1.0"#;
        assert_eq!(kv_tokens_from_cache_config(body), Some(1600));
    }

    #[test]
    fn kv_budget_absent() {
        assert_eq!(kv_tokens_from_cache_config("vllm:other_metric 1.0"), None);
    }
}
