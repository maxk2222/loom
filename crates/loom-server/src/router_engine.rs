//! LLM auto-router: a `kind='router'` virtual model classifies each incoming
//! request with a small, fast router model and forwards it to the best-matching
//! member. Members may be virtual models (`vm:<api_name>`) or direct client
//! deployments (`dep:<uuid>`, materialized as hidden `kind='direct'` models).

use crate::model_registry::{ModelRegistry, VirtualModel};
use crate::scheduler::{PendingRequest, SchedErr, Scheduler};
use anyhow::{bail, Result};
use loom_core::capability::ApiType;
use loom_core::ids::RequestId;
use loom_core::RequiredCaps;
use serde_json::{json, Value};

pub const DEFAULT_PROMPT: &str = "You are a request router for an LLM cluster.\n\
Pick the single best model for the user's request based on the model list below.\n\
Reply with ONLY the model name — no explanation, no punctuation.\n\n\
Available models:\n{{models}}";

/// What the router decided, for tracing/UI.
#[derive(Debug, Clone)]
pub struct RouteChoice {
    /// Virtual-model api_name to dispatch to (vm ref or materialized direct).
    pub model: String,
    /// The member label the router emitted.
    pub name: String,
    /// Raw classifier output (for the test console / debugging).
    pub raw: String,
    pub reason: String,
}

/// Implicit direct-model api_name for a deployment member.
pub fn dep_vm_name(dep_id: &str) -> String {
    format!("dep-{dep_id}")
}

fn cfg_str<'a>(cfg: &'a Value, k: &str) -> &'a str {
    cfg.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Extract the text to classify: last user message (or `prompt` for
/// /v1/completions), truncated to `max_chars` (default 2000).
pub fn user_excerpt(body: &Value, max_chars: usize) -> String {
    let max = if max_chars == 0 { 2000 } else { max_chars };
    let mut text = body["messages"]
        .as_array()
        .and_then(|ms| ms.iter().rev().find(|m| m["role"].as_str() == Some("user")))
        .and_then(|m| {
            m["content"].as_str().map(String::from).or_else(|| {
                // multimodal: take text parts only
                m["content"].as_array().map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| {
                            if p["type"].as_str() == Some("text") {
                                Some(p["text"].as_str().unwrap_or(""))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
        })
        .or_else(|| body["prompt"].as_str().map(String::from))
        .unwrap_or_default();
    if text.chars().count() > max {
        text = text.chars().take(max).collect();
        text.push('…');
    }
    text
}

/// Run the classifier once: builds the routing prompt, submits a tiny
/// non-streaming chat request to `router_model` through the normal scheduler
/// path (queueing, readiness gates, fallbacks all apply), returns raw text.
async fn classify(
    sched: &Scheduler,
    models: &ModelRegistry,
    cfg: &Value,
    excerpt: &str,
    prio: u32,
    deadline_ms: i64,
) -> Result<String> {
    let router_model = cfg_str(cfg, "router_model");
    if router_model.is_empty() {
        bail!("router has no router_model");
    }
    // Recursion guard: the engine model must be a real model, not a router.
    let rvm = models
        .resolve(router_model)
        .await
        .ok_or_else(|| anyhow::anyhow!("router model '{router_model}' not found"))?;
    if rvm.kind == "router" {
        bail!("router model must not itself be a router");
    }
    let mut menu = String::new();
    if let Some(ms) = cfg["members"].as_array() {
        for m in ms {
            let name = m["name"].as_str().unwrap_or("?");
            let desc = m["description"].as_str().unwrap_or("");
            menu.push_str(&format!("- {name}: {desc}\n"));
        }
    }
    let tpl = cfg_str(cfg, "system_prompt");
    let tpl = if tpl.is_empty() { DEFAULT_PROMPT } else { tpl };
    // Both placeholders may appear in the template; {{request}} is also sent
    // as the user message so templates that omit it still work.
    let system = tpl
        .replace("{{models}}", menu.trim_end())
        .replace("{{request}}", excerpt);
    let body = json!({
        "model": router_model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": excerpt},
        ],
        "temperature": 0,
        "max_tokens": 24,
        "stream": false,
    });
    let now = crate::client_registry::now_ms();
    let pending = PendingRequest {
        request_id: RequestId::new(),
        api: ApiType::Chat,
        model: router_model.to_string(),
        body: serde_json::to_vec(&body).unwrap_or_default(),
        required: RequiredCaps::default(),
        project_id: "router".into(),
        prio,
        enqueue_ts: now,
        deadline_ts: now + deadline_ms.min(10_000), // router calls stay cheap
    };
    let out = sched.submit(pending).await.map_err(sched_msg)?;
    let mut rx = out.response_rx;
    let mut buf = Vec::new();
    while let Some(chunk) = rx.recv().await {
        match chunk {
            Ok(b) => buf.extend_from_slice(&b),
            Err(e) => bail!("router call failed: {e}"),
        }
    }
    let resp: Value = serde_json::from_slice(&buf).unwrap_or_default();
    Ok(resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string())
}

fn sched_msg(e: SchedErr) -> anyhow::Error {
    anyhow::anyhow!("router engine dispatch: {e}")
}

/// Map raw router output to a configured member. Exact name match first,
/// then case-insensitive containment — small models add noise around names.
pub fn pick_member(cfg: &Value, raw: &str) -> Option<RouteChoice> {
    let members = cfg["members"].as_array()?;
    let norm = |s: &str| {
        s.trim()
            .trim_matches(|c: char| "\"'`.,;:!".contains(c))
            .to_lowercase()
    };
    let raw_n = norm(raw);
    for m in members {
        if norm(m["name"].as_str().unwrap_or("")) == raw_n {
            return Some(to_choice(m, raw, "exact"));
        }
    }
    // Containment: longest member name first so `fast-code` wins over `fast`.
    let mut by_len: Vec<&Value> = members.iter().collect();
    by_len.sort_by_key(|m| std::cmp::Reverse(m["name"].as_str().unwrap_or("").len()));
    for m in by_len {
        let n = norm(m["name"].as_str().unwrap_or(""));
        if !n.is_empty() && raw_n.contains(&n) {
            return Some(to_choice(m, raw, "contains"));
        }
    }
    None
}

fn to_choice(m: &Value, raw: &str, reason: &str) -> RouteChoice {
    let refv = m["ref"].as_str().unwrap_or("");
    // `resolved` is written by the admin materialization for dep: refs and
    // equals dep_vm_name(dep) — computing it keeps the engine DB-free.
    let model = m["resolved"]
        .as_str()
        .map(String::from)
        .or_else(|| {
            refv.strip_prefix("vm:")
                .map(String::from)
                .or_else(|| refv.strip_prefix("dep:").map(dep_vm_name))
        })
        .unwrap_or_default();
    RouteChoice {
        model,
        name: m["name"].as_str().unwrap_or("").into(),
        raw: raw.into(),
        reason: reason.into(),
    }
}

/// Full router pass: excerpt → classify → pick → fallback on failure.
/// Returns the chosen target; `Err` only when nothing is routable at all.
pub async fn route(
    sched: &Scheduler,
    models: &ModelRegistry,
    vm: &VirtualModel,
    excerpt: &str,
    prio: u32,
    deadline_ms: i64,
) -> Result<RouteChoice> {
    let cfg = &vm.router_config;
    let raw = classify(sched, models, cfg, excerpt, prio, deadline_ms).await;
    match raw {
        Ok(text) => {
            if let Some(ch) = pick_member(cfg, &text) {
                return Ok(ch);
            }
            let fb = cfg_str(cfg, "fallback_model");
            if !fb.is_empty() {
                return Ok(RouteChoice {
                    model: fb.into(),
                    name: fb.into(),
                    raw: text,
                    reason: "unparsed→fallback".into(),
                });
            }
            bail!("router chose unknown model '{text}' and no fallback set");
        }
        Err(e) => {
            let fb = cfg_str(cfg, "fallback_model");
            if !fb.is_empty() {
                return Ok(RouteChoice {
                    model: fb.into(),
                    name: fb.into(),
                    raw: e.to_string(),
                    reason: "engine-error→fallback".into(),
                });
            }
            Err(e)
        }
    }
}

/// For the admin test console: run classification only, no dispatch.
pub async fn test_classify(
    sched: &Scheduler,
    models: &ModelRegistry,
    vm: &VirtualModel,
    excerpt: &str,
    deadline_ms: i64,
) -> Value {
    let cfg = &vm.router_config;
    match classify(sched, models, cfg, excerpt, 1, deadline_ms).await {
        Ok(text) => json!({
            "ok": true,
            "raw": text,
            "pick": pick_member(cfg, &text).map(|c| json!({"model": c.model, "name": c.name, "reason": c.reason})),
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

/// Convenience used by gateway: does `model` resolve to a router?
pub async fn is_router(models: &ModelRegistry, model: &str) -> Option<VirtualModel> {
    let vm = models.resolve(model).await?;
    (vm.kind == "router").then_some(vm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Value {
        json!({
            "members": [
                {"ref": "vm:fast", "name": "fast", "description": "quick answers"},
                {"ref": "vm:fast-code", "name": "fast-code", "description": "coding"},
                {"ref": "dep:abc-123", "resolved": "dep-abc-123", "name": "qwen", "description": "multilingual"},
            ]
        })
    }

    #[test]
    fn picks_exact_name() {
        let c = cfg();
        let ch = pick_member(&c, "fast").unwrap();
        assert_eq!(ch.model, "fast");
        assert_eq!(ch.reason, "exact");
    }

    #[test]
    fn picks_longest_contained() {
        let c = cfg();
        let ch = pick_member(&c, "I choose fast-code for this").unwrap();
        assert_eq!(ch.model, "fast-code");
    }

    #[test]
    fn maps_dep_ref_to_direct_vm() {
        let c = cfg();
        let ch = pick_member(&c, "qwen").unwrap();
        assert_eq!(ch.model, "dep-abc-123");
    }

    #[test]
    fn noisy_output_still_matches() {
        let c = cfg();
        let ch = pick_member(&c, "\"fast\".").unwrap();
        assert_eq!(ch.model, "fast");
    }

    #[test]
    fn excerpt_takes_last_user_message() {
        let body = json!({"messages":[
            {"role":"system","content":"sys"},
            {"role":"user","content":"first"},
            {"role":"assistant","content":"ans"},
            {"role":"user","content":"last question"},
        ]});
        assert_eq!(user_excerpt(&body, 2000), "last question");
    }

    #[test]
    fn excerpt_multimodal_and_truncates() {
        let body = json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"describe"},
            {"type":"image_url","image_url":{"url":"x"}},
            {"type":"text","text":"this image"},
        ]}]});
        assert_eq!(user_excerpt(&body, 2000), "describe this image");
        let long = json!({"prompt": "x".repeat(100)});
        assert_eq!(user_excerpt(&long, 10), "xxxxxxxxxx…");
    }
}
