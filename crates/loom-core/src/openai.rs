//! OpenAI-compatible API types (chat completions, completions, embeddings,
//! models). Request types are permissive: unknown fields are preserved via
//! `extra` flatten maps so newer OpenAI parameters pass through to backends.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Chat completions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>, // string | array of content parts | null
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: ChatMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
}

/// Streaming chunk (one SSE `data:` payload).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunkChoice {
    pub index: u32,
    #[serde(default)]
    pub delta: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
}

// ---------------------------------------------------------------------------
// Legacy completions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub prompt: Value, // string | array
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Embeddings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Value, // string | array<string> | array<tokens>
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub data: Vec<EmbeddingObject>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingObject {
    pub object: String,
    pub index: u32,
    pub embedding: Value,
}

// ---------------------------------------------------------------------------
// Models list
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelList {
    pub object: String,
    pub data: Vec<ModelCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCard {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ModelList {
    pub fn of(models: Vec<String>, owned_by: &str) -> Self {
        let created = chrono::Utc::now().timestamp();
        Self {
            object: "list".into(),
            data: models
                .into_iter()
                .map(|id| ModelCard {
                    id,
                    object: "model".into(),
                    created,
                    owned_by: owned_by.into(),
                    extra: Map::new(),
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Request analysis helpers (capability derivation — no content retained)
// ---------------------------------------------------------------------------

/// Derive required capabilities from a raw request JSON.
pub fn required_caps_from_request(body: &Value) -> crate::RequiredCaps {
    let mut req = crate::RequiredCaps::default();
    if body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        == Some(true)
        || body
            .get("functions")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            == Some(true)
    {
        req.tools = true;
    }
    if body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        req.streaming = true;
    }
    if let Some(fmt) = body.get("response_format") {
        if fmt.get("type").and_then(|v| v.as_str()) == Some("json_object")
            || fmt.get("type").and_then(|v| v.as_str()) == Some("json_schema")
        {
            req.json_mode = true;
        }
    }
    // vision: any message content part with type image_url / input_image / image
    // video: any content part carrying video input (video_url / input_video / video)
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if let Some(parts) = m.get("content").and_then(|c| c.as_array()) {
                for p in parts {
                    let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if t.contains("video") {
                        req.video = true;
                    } else if t.contains("image") || t == "image_url" || t == "input_image" {
                        req.vision = true;
                    }
                }
            }
            if req.vision && req.video {
                break;
            }
        }
    }
    req
}

/// The model field of any OpenAI request body.
pub fn request_model(body: &Value) -> Option<&str> {
    body.get("model").and_then(|m| m.as_str())
}

/// Whether the request asked for streaming.
pub fn request_stream(body: &Value) -> bool {
    body.get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// SSE frame prefix helper.
pub fn sse_data_line(v: &Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(v).unwrap_or_else(|_| json!({}).to_string())
    )
}

pub const SSE_DONE: &str = "data: [DONE]\n\n";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_video_input_parts() {
        let body = json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"describe"},
            {"type":"video_url","video_url":{"url":"x.mp4"}}
        ]}]});
        let req = required_caps_from_request(&body);
        assert!(req.video);
        assert!(!req.vision);

        // input_video / video spellings count too
        for t in ["input_video", "video"] {
            let body = json!({"messages":[{"role":"user","content":[{"type":t}]}]});
            assert!(required_caps_from_request(&body).video, "type {t}");
        }
        // mixed image+video sets both
        let body = json!({"messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"x"}},
            {"type":"video_url","video_url":{"url":"x"}}
        ]}]});
        let req = required_caps_from_request(&body);
        assert!(req.vision && req.video);
    }

    #[test]
    fn satisfies_gates_video_input() {
        use crate::capability::{Capabilities, RequiredCaps};
        let req = RequiredCaps {
            video: true,
            ..Default::default()
        };
        // text-only deployment fails
        assert!(!Capabilities::default().satisfies(&req));
        // vision-only image VLM fails — video needs explicit support
        let image_vlm = Capabilities {
            vision: true,
            ..Default::default()
        };
        assert!(!image_vlm.satisfies(&req));
        // video_input flag satisfies
        let v = Capabilities {
            video_input: true,
            ..Default::default()
        };
        assert!(v.satisfies(&req));
        // "video" in modalities satisfies too
        let m = Capabilities {
            modalities: ["video".to_string()].into_iter().collect(),
            ..Default::default()
        };
        assert!(m.satisfies(&req));
    }
}
