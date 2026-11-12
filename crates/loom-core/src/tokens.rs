//! Lightweight token estimation.
//!
//! Backends normally report exact `usage` counters; this module exists for the
//! cases where they do not (streaming without usage, error paths, pre-dispatch
//! admission decisions). It intentionally stays dependency-free: a real
//! per-model tokenizer would require shipping/fetching tokenizer.json files,
//! which LM Studio/vLLM deployments do not expose via the OpenAI API.
//!
//! Heuristic: ~1 token per 3.5 chars for latin text, plus structural overhead
//! per chat message. Accuracy is roughly ±20% vs. BPE tokenizers — fine for
//! rate-limit estimation and usage fallback, not for billing.

/// Estimate tokens for a text blob.
pub fn estimate_tokens(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    // Mixed-language safe lower bound: CJK chars are ~1 token each, latin ~1/4.
    let cjk = text
        .chars()
        .filter(|c| {
            matches!(
                *c as u32,
                0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
            )
        })
        .count() as u32;
    let latin = chars.saturating_sub(cjk);
    (latin / 4 + cjk + 1).max(1)
}

/// Estimate prompt tokens of an OpenAI chat/completions request body.
/// Counts everything that actually reaches the model: message contents,
/// reasoning blocks, tool calls AND tool results, image/audio parts,
/// tool schemas, and legacy function definitions.
/// Falls back to embedding-shaped `input` fields for embeddings requests.
pub fn estimate_request_tokens(body: &serde_json::Value) -> u32 {
    // Chat: sum message contents + ~4 tokens structural overhead per message.
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        let mut t = 0u32;
        for m in msgs {
            // Per-message framing: role + name/tool_call_id wrappers.
            t += 4;
            if let Some(s) = m.get("name").and_then(|v| v.as_str()) {
                t += estimate_tokens(s) + 1;
            }
            if let Some(s) = m.get("tool_call_id").and_then(|v| v.as_str()) {
                t += estimate_tokens(s) + 1;
            }
            match m.get("content") {
                Some(serde_json::Value::String(s)) => t += estimate_tokens(s),
                Some(serde_json::Value::Array(parts)) => {
                    for p in parts {
                        t += estimate_content_part(p);
                    }
                }
                _ => {}
            }
            // Reasoning models echo their chain-of-thought into the context
            // (Qwen `reasoning_content`, DeepSeek `thinking`, etc.) — it is
            // re-fed on every turn, so it counts as prompt tokens.
            for key in ["reasoning_content", "thinking", "reasoning"] {
                if let Some(s) = m.get(key).and_then(|v| v.as_str()) {
                    t += estimate_tokens(s);
                }
            }
            // Assistant tool_calls (name + arguments JSON) are prompt tokens.
            if let Some(tc) = m.get("tool_calls") {
                t += estimate_tokens(&tc.to_string());
            }
            // Legacy function_call.
            if let Some(fc) = m.get("function_call") {
                t += estimate_tokens(&fc.to_string());
            }
        }
        // Tool/function schemas are injected into the prompt.
        for key in ["tools", "functions"] {
            if let Some(v) = body.get(key) {
                t += estimate_tokens(&v.to_string());
            }
        }
        // Structured-output schema (response_format.json_schema).
        if let Some(v) = body.get("response_format") {
            t += estimate_tokens(&v.to_string()) / 2;
        }
        return t.max(1);
    }
    // Legacy completions: `prompt` string or list.
    if let Some(p) = body.get("prompt") {
        return match p {
            serde_json::Value::String(s) => estimate_tokens(s),
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(estimate_tokens))
                .sum(),
            _ => 0,
        };
    }
    // Embeddings: `input` string or list.
    if let Some(i) = body.get("input") {
        return match i {
            serde_json::Value::String(s) => estimate_tokens(s),
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(estimate_tokens))
                .sum(),
            _ => 0,
        };
    }
    0
}

/// Request KV footprint: the tokens a lease should reserve on a deployment.
/// `prompt estimate + min(requested output, output_reserve) + margin%`.
/// `max_tokens`/`max_completion_tokens` cap the output reservation; absent
/// values fall back to `output_reserve` so under-specified requests still
/// reserve honest headroom. The margin absorbs estimate error (±20% BPE
/// drift) — it is intentionally part of the reservation rather than a
/// separate budget bump so `kv_used` stays the sum of lease footprints.
pub fn request_footprint(body: &serde_json::Value, output_reserve: u64, margin_pct: u64) -> u64 {
    let prompt = estimate_request_tokens(body) as u64;
    let requested = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(output_reserve);
    let out = requested.min(output_reserve);
    let base = prompt + out;
    base + (base * margin_pct / 100)
}

/// Flat footprint for non-text apis (embeddings, images, audio, video):
/// no KV growth, but the lease still occupies scheduler bookkeeping.
pub fn flat_footprint() -> u64 {
    512
}

/// Estimate tokens for one part of a multimodal `content` array.
/// Image numbers follow the OpenAI tiling convention (low≈85, high≈765+),
/// which is also a reasonable proxy for how LM Studio/vLLM account vision
/// inputs. Audio is estimated ~25 tokens/second-equivalent when unknown.
fn estimate_content_part(p: &serde_json::Value) -> u32 {
    if let Some(txt) = p.get("text").and_then(|v| v.as_str()) {
        return estimate_tokens(txt);
    }
    let ty = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "image_url" | "image" | "input_image" => {
            let detail = p
                .get("image_url")
                .and_then(|i| i.get("detail"))
                .and_then(|d| d.as_str())
                .or_else(|| p.get("detail").and_then(|d| d.as_str()))
                .unwrap_or("auto");
            match detail {
                "low" => 85,
                _ => 765,
            }
        }
        "input_audio" | "audio" => {
            // No reliable duration without decoding; assume a short clip.
            200
        }
        "file" | "input_file" => {
            // Inline file data (base64) — estimate by payload size, cap it.
            let sz = p
                .get("file")
                .and_then(|f| f.get("file_data"))
                .and_then(|d| d.as_str())
                .map(|d| d.len() as u32)
                .unwrap_or(0);
            (sz / 4 / 4).min(4000) + 10 // ~base64→bytes→tokens, capped
        }
        // Unknown future part types still carry something — count the JSON.
        _ => estimate_tokens(&p.to_string()).min(512),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_latin_text() {
        assert!(estimate_tokens("hello world, this is a test") > 4);
    }

    #[test]
    fn chat_overhead_counts_messages() {
        let body = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi there"},
                {"role": "user", "content": "another message here"}
            ]
        });
        let t = estimate_request_tokens(&body);
        assert!((10..=40).contains(&t), "t={t}");
    }

    #[test]
    fn embeddings_input() {
        let body = serde_json::json!({"input": "some text to embed"});
        assert!(estimate_request_tokens(&body) >= 3);
    }

    #[test]
    fn counts_tool_results_and_calls() {
        let body = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"berlin\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1",
                 "content": "{\"temp\": 21, \"cond\": \"sunny\", \"wind_kph\": 12}"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}]
        });
        let t = estimate_request_tokens(&body);
        // tool schema + call + result + messages → well above bare-text count.
        assert!(t >= 60, "t={t}");
    }

    #[test]
    fn counts_reasoning_and_images() {
        let body = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": "answer",
                 "reasoning_content": "let me think about this step by step"},
                {"role": "user", "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image_url", "image_url": {"url": "data:...", "detail": "high"}}
                ]}
            ]
        });
        let t = estimate_request_tokens(&body);
        assert!(t >= 780, "t={t}"); // 765 image + reasoning + text + overhead
    }
}
