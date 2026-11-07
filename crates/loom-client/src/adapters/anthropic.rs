//! Anthropic Messages API adapter.
//!
//! Anthropic speaks a different protocol (`POST /v1/messages`,
//! `x-api-key` + `anthropic-version` headers, different request/response
//! schema, own SSE event model). This adapter translates OpenAI chat
//! requests to Anthropic and rewrites responses (JSON + SSE) back to
//! OpenAI chat.completion format, so the rest of LOOM stays protocol-clean.
//!
//! Scope: chat only (completions/embeddings are rejected). Text, image
//! and tool content blocks are translated; reasoning (`thinking`) is not
//! exposed to keep the mapping deterministic.

use super::common;
use super::{AdapterError, BackendAdapter, BackendResponse, ByteStream, Health};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::BackendConfig;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicAdapter {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl AnthropicAdapter {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
    }
}

#[async_trait]
impl BackendAdapter for AnthropicAdapter {
    fn backend_type(&self) -> BackendType {
        BackendType::Anthropic
    }

    async fn list_models(&self) -> Result<Vec<String>, AdapterError> {
        let req = self.auth(
            self.http
                .get(format!("{}/v1/models", self.endpoint()))
                .timeout(Duration::from_secs(10)),
        );
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

    async fn probe_capabilities(&self, _model: &str) -> Result<Capabilities, AdapterError> {
        // Claude models are multimodal + tool-capable; no json_mode flag,
        // no embeddings, no legacy completions in the Messages API.
        Ok(Capabilities {
            chat: true,
            completions: false,
            embeddings: false,
            tools: true,
            vision: true,
            streaming: true,
            json_mode: false,
            ..Default::default()
        })
    }

    async fn health(&self) -> Health {
        match self.list_models().await {
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
        if api != ApiType::Chat {
            return Err(AdapterError::Protocol(format!(
                "anthropic backend does not support the {api} api"
            )));
        }
        let streaming = body
            .get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        let mut anth = openai_to_anthropic(&body);
        anth["model"] = json!(model);

        let req = self.auth(
            self.http
                .post(format!("{}/v1/messages", self.endpoint()))
                .json(&anth)
                .timeout(Duration::from_secs(600)),
        );
        let resp = common::send_stream(req, cancel).await?;
        let status = resp.status;

        if !streaming {
            // Buffer the whole JSON body, translate, emit as a single chunk.
            let mut s = resp.stream;
            let mut raw = Vec::new();
            while let Some(item) = s.next().await {
                match item {
                    Ok(b) => raw.extend_from_slice(&b),
                    Err(e) => return Err(e),
                }
            }
            let out = if status == 200 {
                let v: Value = serde_json::from_slice(&raw)
                    .map_err(|e| AdapterError::Protocol(format!("anthropic json: {e}")))?;
                serde_json::to_vec(&anthropic_to_openai(&v, model))
                    .map_err(|e| AdapterError::Protocol(e.to_string()))?
            } else {
                raw // pass error bodies through untouched
            };
            let once: ByteStream = Box::pin(stream::once(async move { Ok(Bytes::from(out)) }));
            return Ok(BackendResponse {
                status,
                is_event_stream: false,
                stream: once,
                content_type: "application/json".into(),
            });
        }

        // Streaming: rewrite Anthropic SSE events to OpenAI chunks.
        let translator = AnthropicSse::new(model);
        let init = (resp.stream, translator, VecDeque::<Bytes>::new(), false);
        let out_stream = stream::unfold(init, |(mut s, mut t, mut q, mut done)| async move {
            loop {
                if let Some(b) = q.pop_front() {
                    return Some((Ok(b), (s, t, q, done)));
                }
                if done {
                    return None;
                }
                match s.next().await {
                    Some(Ok(chunk)) => {
                        for f in t.push(&chunk) {
                            q.push_back(Bytes::from(f));
                        }
                    }
                    Some(Err(e)) => return Some((Err(e), (s, t, q, done))),
                    None => {
                        for f in t.finish() {
                            q.push_back(Bytes::from(f));
                        }
                        done = true;
                    }
                }
            }
        });
        Ok(BackendResponse {
            status,
            is_event_stream: true,
            stream: Box::pin(out_stream),
            content_type: "text/event-stream".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Request translation (OpenAI chat → Anthropic messages)
// ---------------------------------------------------------------------------

/// Convert an OpenAI chat.completions request body into an Anthropic
/// /v1/messages request body.
pub fn openai_to_anthropic(body: &Value) -> Value {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for m in body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(t) = text_of(&m) {
                    system.push(json!({"type": "text", "text": t}));
                }
            }
            "tool" => {
                // OpenAI tool result → Anthropic user message with tool_result.
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                        "content": text_of(&m).unwrap_or_default(),
                    }]
                }));
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(t) = text_of(&m) {
                    parts.push(json!({"type": "text", "text": t}));
                }
                for tc in m
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    let f = tc.get("function").cloned().unwrap_or_default();
                    let args: Value = f
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_else(|| f.get("arguments").cloned().unwrap_or(json!({})));
                    parts.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                        "name": f.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "input": args,
                    }));
                }
                if !parts.is_empty() {
                    messages.push(json!({"role": "assistant", "content": parts}));
                }
            }
            _ => {
                messages.push(json!({
                    "role": "user",
                    "content": content_parts(&m),
                }));
            }
        }
    }

    let mut out = json!({ "messages": messages });
    if !system.is_empty() {
        out["system"] = Value::Array(system);
    }
    // Anthropic REQUIRES max_tokens.
    let max = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);
    out["max_tokens"] = json!(max);
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        out["stream"] = json!(true);
    }
    for (k, ak) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("top_k", "top_k"),
    ] {
        if let Some(v) = body.get(k) {
            out[ak] = v.clone();
        }
    }
    if let Some(stop) = body.get("stop") {
        let seq: Vec<Value> = match stop {
            Value::String(s) => vec![json!(s)],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        if !seq.is_empty() {
            out["stop_sequences"] = Value::Array(seq);
        }
    }
    // tools: {type:function,function:{name,description,parameters}}
    //      → {name, description, input_schema}
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let at: Vec<Value> = tools
            .iter()
            .map(|t| {
                let f = t.get("function").cloned().unwrap_or_else(|| t.clone());
                json!({
                    "name": f.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "description": f.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "input_schema": f.get("parameters").cloned().unwrap_or(json!({"type":"object"})),
                })
            })
            .collect();
        if !at.is_empty() {
            out["tools"] = Value::Array(at);
        }
    }
    out
}

/// Concatenated plain text of a message's content (string or text parts).
fn text_of(m: &Value) -> Option<String> {
    match m.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let t: String = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("");
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        _ => None,
    }
}

/// OpenAI content (string | multimodal parts) → Anthropic content blocks.
fn content_parts(m: &Value) -> Vec<Value> {
    match m.get("content") {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| {
                let ty = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "image_url" | "input_image" | "image" => {
                        let url = p
                            .get("image_url")
                            .and_then(|i| i.get("url"))
                            .and_then(|u| u.as_str())
                            .or_else(|| p.get("url").and_then(|u| u.as_str()))
                            .unwrap_or("");
                        if let Some(b64) = url.strip_prefix("data:").and_then(|d| d.split(",").nth(1))
                        {
                            let mt = url
                                .strip_prefix("data:")
                                .and_then(|d| d.split(";").next())
                                .unwrap_or("image/png");
                            json!({"type":"image","source":{"type":"base64","media_type":mt,"data":b64}})
                        } else {
                            json!({"type":"image","source":{"type":"url","url":url}})
                        }
                    }
                    _ => json!({"type": "text", "text": p.get("text").and_then(|v| v.as_str()).unwrap_or("")}),
                }
            })
            .collect(),
        _ => vec![json!({"type": "text", "text": ""})],
    }
}

// ---------------------------------------------------------------------------
// Response translation (Anthropic → OpenAI)
// ---------------------------------------------------------------------------

fn finish_reason(stop: &str) -> &'static str {
    match stop {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

/// Anthropic non-stream message → OpenAI chat.completion.
pub fn anthropic_to_openai(v: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for (i, c) in v
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        match c.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => text.push_str(c.get("text").and_then(|t| t.as_str()).unwrap_or("")),
            "tool_use" => tool_calls.push(json!({
                "index": i,
                "id": c.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "type": "function",
                "function": {
                    "name": c.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "arguments": c.get("input").map(|i| i.to_string()).unwrap_or("{}".into()),
                },
            })),
            _ => {}
        }
    }
    let mut msg = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
        msg["content"] = if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        };
    }
    let u = v.get("usage").cloned().unwrap_or_default();
    let pt = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let ct = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    json!({
        "id": v.get("id").and_then(|i| i.as_str()).unwrap_or("chatcmpl-anthropic"),
        "object": "chat.completion",
        "created": chrono_now(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": msg,
            "finish_reason": finish_reason(v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("")),
        }],
        "usage": {
            "prompt_tokens": pt,
            "completion_tokens": ct,
            "total_tokens": pt + ct,
        },
    })
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// SSE translation: Anthropic event stream → OpenAI chat.completion.chunk
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BlockState {
    kind: String, // "text" | "tool_use"
    tool_index: usize,
    tool_id: String,
}

/// Incremental translator: feed raw upstream bytes, get OpenAI SSE frames out.
pub struct AnthropicSse {
    model: String,
    id: String,
    buf: String,
    role_sent: bool,
    blocks: HashMap<usize, BlockState>,
    input_tokens: u64,
    output_tokens: u64,
    finished: bool,
    finish_reason: String,
}

impl AnthropicSse {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            id: "chatcmpl-anthropic".into(),
            buf: String::new(),
            role_sent: false,
            blocks: HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            finished: false,
            finish_reason: String::new(),
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        format!(
            "data: {}\n\n",
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": chrono_now(),
                "model": self.model,
                "choices": [{
                    "index": 0,
                    "delta": delta,
                    "finish_reason": finish,
                }],
            })
        )
    }

    fn role_delta(&mut self) -> Option<String> {
        if self.role_sent {
            return None;
        }
        self.role_sent = true;
        Some(self.chunk(json!({"role": "assistant"}), None))
    }

    /// Feed one upstream byte chunk; returns complete OpenAI SSE frames.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = self.buf.find("\n\n") {
            let event = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            self.handle_event(&event, &mut out);
        }
        out
    }

    /// Flush remaining buffer, emit usage + [DONE].
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.buf.trim().is_empty() {
            let ev = self.buf.clone();
            self.buf.clear();
            self.handle_event(&ev, &mut out);
        }
        if !self.finished {
            self.finished = true;
            let fr = if self.finish_reason.is_empty() {
                "stop"
            } else {
                &self.finish_reason.clone()
            };
            out.push(self.chunk(json!({}), Some(fr)));
        }
        out.push(format!(
            "data: {}\n\n",
            json!({
                "id": self.id, "object": "chat.completion.chunk",
                "created": chrono_now(), "model": self.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": self.input_tokens,
                    "completion_tokens": self.output_tokens,
                    "total_tokens": self.input_tokens + self.output_tokens,
                },
            })
        ));
        out.push("data: [DONE]\n\n".to_string());
        out
    }

    fn handle_event(&mut self, event: &str, out: &mut Vec<String>) {
        let mut ty = "";
        let mut data = "";
        for line in event.lines() {
            if let Some(t) = line.strip_prefix("event:") {
                ty = t.trim();
            } else if let Some(d) = line.strip_prefix("data:") {
                data = d.trim();
            }
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match ty {
            "message_start" => {
                if let Some(m) = v.get("message") {
                    if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                        self.id = id.to_string();
                    }
                    self.input_tokens = m
                        .get("usage")
                        .and_then(|u| u.get("input_tokens"))
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0);
                }
                if let Some(f) = self.role_delta() {
                    out.push(f);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let cb = v.get("content_block").cloned().unwrap_or_default();
                let kind = cb
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut st = BlockState {
                    kind: kind.clone(),
                    ..Default::default()
                };
                if kind == "tool_use" {
                    st.tool_index = self
                        .blocks
                        .values()
                        .filter(|b| b.kind == "tool_use")
                        .count();
                    st.tool_id = cb
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(self.chunk(
                        json!({"tool_calls": [{
                            "index": st.tool_index,
                            "id": st.tool_id,
                            "type": "function",
                            "function": {
                                "name": cb.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                "arguments": "",
                            },
                        }]}),
                        None,
                    ));
                }
                self.blocks.insert(idx, st);
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let d = v.get("delta").cloned().unwrap_or_default();
                match d.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text_delta" => {
                        out.push(self.chunk(
                            json!({"content": d.get("text").and_then(|t| t.as_str()).unwrap_or("")}),
                            None,
                        ));
                    }
                    "input_json_delta" => {
                        let ti = self.blocks.get(&idx).map(|b| b.tool_index).unwrap_or(0);
                        out.push(self.chunk(
                            json!({"tool_calls": [{
                                "index": ti,
                                "function": {
                                    "arguments": d.get("partial_json").and_then(|t| t.as_str()).unwrap_or(""),
                                },
                            }]}),
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.finish_reason = finish_reason(sr).to_string();
                }
                self.output_tokens = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(self.output_tokens);
            }
            "message_stop" => {
                self.finished = true;
                let fr = if self.finish_reason.is_empty() {
                    "stop"
                } else {
                    &self.finish_reason.clone()
                };
                out.push(self.chunk(json!({}), Some(fr)));
            }
            _ => {} // ping, content_block_stop, error handled via status
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_translation_system_tools() {
        let body = json!({
            "model": "claude",
            "stream": true,
            "max_tokens": 100,
            "temperature": 0.5,
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": {"name": "get_w", "arguments": "{\"city\":\"b\"}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "sunny"}
            ],
            "tools": [{"type":"function","function":{"name":"get_w","description":"w","parameters":{"type":"object"}}}]
        });
        let a = openai_to_anthropic(&body);
        assert_eq!(a["max_tokens"], 100);
        assert_eq!(a["stream"], true);
        assert_eq!(a["system"][0]["text"], "be terse");
        assert_eq!(a["messages"].as_array().unwrap().len(), 3);
        assert_eq!(a["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(a["messages"][1]["content"][0]["input"]["city"], "b");
        assert_eq!(a["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(a["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn response_translation() {
        let v = json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"f","input":{"a":1}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let o = anthropic_to_openai(&v, "claude");
        assert_eq!(o["usage"]["prompt_tokens"], 10);
        assert_eq!(o["usage"]["completion_tokens"], 5);
        assert_eq!(o["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            o["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "f"
        );
    }

    #[test]
    fn sse_translation_text_and_tool() {
        let mut t = AnthropicSse::new("claude");
        let stream_in = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"usage\":{\"input_tokens\":7}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"f\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut out = t.push(stream_in.as_bytes());
        out.extend(t.finish());
        let joined = out.join("");
        assert!(joined.contains("\"role\":\"assistant\""));
        assert!(joined.contains("\"content\":\"hello\""));
        assert!(joined.contains("\"name\":\"f\""));
        assert!(joined.contains("\"finish_reason\":\"tool_calls\""));
        assert!(joined.contains("\"prompt_tokens\":7"));
        assert!(joined.contains("\"completion_tokens\":3"));
        assert!(joined.contains("[DONE]"));
    }
}
