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
        content_type: "application/json".into(),
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

/// Request signals used by `mode: "heuristic"` routers — cheap and fully
/// deterministic, no engine call needed.
#[derive(Debug, Default)]
pub struct ReqMetrics {
    /// Chars of the last user message text.
    pub chars: usize,
    /// Message count.
    pub msgs: usize,
    /// Code fences / language hints in the request text.
    pub code: bool,
    /// Multimodal image parts present.
    pub images: bool,
    /// A system message is present.
    pub system: bool,
}

/// Compute request metrics from the raw OpenAI body.
pub fn req_metrics(body: &Value) -> ReqMetrics {
    let mut m = ReqMetrics::default();
    if let Some(ms) = body["messages"].as_array() {
        m.msgs = ms.len();
        let mut all_text = String::new();
        for msg in ms {
            if msg["role"].as_str() == Some("system") {
                m.system = true;
            }
            match &msg["content"] {
                Value::String(t) => all_text.push_str(t),
                Value::Array(parts) => {
                    for p in parts {
                        match p["type"].as_str() {
                            Some("text") => all_text.push_str(p["text"].as_str().unwrap_or("")),
                            Some("image_url") | Some("image") => m.images = true,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        m.code = looks_like_code(&all_text);
    }
    m.chars = user_excerpt(body, usize::MAX).chars().count();
    // /v1/completions style: single prompt field
    if m.msgs == 0 {
        if let Some(p) = body["prompt"].as_str() {
            m.chars = p.chars().count();
            m.code = looks_like_code(p);
        }
    }
    m
}

fn looks_like_code(t: &str) -> bool {
    t.contains("```")
        || t.contains("#include")
        || (t.contains("fn ") && t.contains('{'))
        || (t.contains("def ") && t.contains(':'))
        || (t.contains("class ") && t.contains('{'))
        || t.contains("function ")
        || (t.contains("import ") && (t.contains("from ") || t.contains(';')))
        || (t.to_uppercase().contains("SELECT ") && t.to_uppercase().contains(" FROM "))
}

fn metrics_summary(m: &ReqMetrics) -> String {
    format!(
        "chars={} msgs={} code={} images={} system={}",
        m.chars, m.msgs, m.code, m.images, m.system
    )
}

/// Evaluate a member `when` rule against request metrics — every listed key
/// must match (AND); an empty/absent rule is the catch-all member.
fn when_matches(when: &Value, m: &ReqMetrics) -> bool {
    let Some(o) = when.as_object() else {
        return when.is_null();
    };
    for (k, v) in o {
        let ok = match k.as_str() {
            "chars_lt" => v.as_u64().is_some_and(|n| (m.chars as u64) < n),
            "chars_gte" => v.as_u64().is_some_and(|n| (m.chars as u64) >= n),
            "msgs_lt" => v.as_u64().is_some_and(|n| (m.msgs as u64) < n),
            "msgs_gte" => v.as_u64().is_some_and(|n| (m.msgs as u64) >= n),
            "code" => v.as_bool().is_some_and(|b| m.code == b),
            "images" => v.as_bool().is_some_and(|b| m.images == b),
            "system" => v.as_bool().is_some_and(|b| m.system == b),
            _ => false, // unknown rule keys never match
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Deterministic pick: first member whose `when` matches, in config order.
fn heuristic_choice(cfg: &Value, m: &ReqMetrics) -> Result<RouteChoice> {
    let summary = metrics_summary(m);
    if let Some(members) = cfg["members"].as_array() {
        for mem in members {
            let when = &mem["when"];
            if when_matches(when, m) {
                let catch_all = when.as_object().map(|o| o.is_empty()).unwrap_or(true);
                return Ok(to_choice(
                    mem,
                    &summary,
                    if catch_all {
                        "heuristic:default"
                    } else {
                        "heuristic"
                    },
                ));
            }
        }
    }
    let fb = cfg_str(cfg, "fallback_model");
    if !fb.is_empty() {
        return Ok(RouteChoice {
            model: fb.into(),
            name: fb.into(),
            raw: summary,
            reason: "heuristic:unmatched→fallback".into(),
        });
    }
    bail!("heuristic router: no member matched and no fallback set ({summary})");
}

/// Full router pass: excerpt → classify → pick → fallback on failure.
/// `mode: "heuristic"` skips the engine call entirely and applies the
/// members' `when` rules to request metrics instead.
/// Returns the chosen target; `Err` only when nothing is routable at all.
pub async fn route(
    sched: &Scheduler,
    models: &ModelRegistry,
    vm: &VirtualModel,
    body: &Value,
    prio: u32,
    deadline_ms: i64,
) -> Result<RouteChoice> {
    let cfg = &vm.router_config;
    if cfg_str(cfg, "mode") == "heuristic" {
        return heuristic_choice(cfg, &req_metrics(body));
    }
    let max_chars = cfg["max_user_chars"].as_u64().unwrap_or(2000) as usize;
    let excerpt = user_excerpt(body, max_chars);
    let raw = classify(sched, models, cfg, &excerpt, prio, deadline_ms).await;
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
/// Heuristic routers need no engine call — the rule evaluation is returned
/// directly with the computed metrics as `raw`.
pub async fn test_classify(
    sched: &Scheduler,
    models: &ModelRegistry,
    vm: &VirtualModel,
    prompt: &str,
    deadline_ms: i64,
) -> Value {
    let cfg = &vm.router_config;
    if cfg_str(cfg, "mode") == "heuristic" {
        let body = json!({"messages": [{"role": "user", "content": prompt}]});
        let m = req_metrics(&body);
        return match heuristic_choice(cfg, &m) {
            Ok(ch) => json!({
                "ok": true,
                "raw": metrics_summary(&m),
                "pick": {"model": ch.model, "name": ch.name, "reason": ch.reason},
            }),
            Err(e) => json!({"ok": false, "error": e.to_string(), "raw": metrics_summary(&m)}),
        };
    }
    match classify(sched, models, cfg, prompt, 1, deadline_ms).await {
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

    fn hcfg() -> Value {
        json!({
            "mode": "heuristic",
            "fallback_model": "vm-fallback",
            "members": [
                {"ref": "vm:vision", "name": "vision", "when": {"images": true}},
                {"ref": "vm:coder", "name": "coder", "when": {"code": true}},
                {"ref": "vm:big", "name": "big", "when": {"chars_gte": 500}},
                {"ref": "vm:fast", "name": "fast", "when": {}}
            ]
        })
    }

    #[test]
    fn heuristic_first_match_wins_in_order() {
        let c = hcfg();
        // code + long: coder listed before big → coder wins
        let body =
            json!({"messages":[{"role":"user","content":format!("```rust\n{}", "x".repeat(600))}]});
        let m = req_metrics(&body);
        assert!(m.code && m.chars >= 500);
        let ch = heuristic_choice(&c, &m).unwrap();
        assert_eq!(ch.model, "coder");
        assert_eq!(ch.reason, "heuristic");
    }

    #[test]
    fn heuristic_images_and_catchall() {
        let c = hcfg();
        let img = json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"u"}}]}]});
        let ch = heuristic_choice(&c, &req_metrics(&img)).unwrap();
        assert_eq!(ch.model, "vision");
        let plain = json!({"messages":[{"role":"user","content":"hi"}]});
        let ch = heuristic_choice(&c, &req_metrics(&plain)).unwrap();
        assert_eq!(ch.model, "fast");
        assert_eq!(ch.reason, "heuristic:default");
    }

    #[test]
    fn heuristic_unmatched_falls_back() {
        let c = json!({
            "mode": "heuristic",
            "fallback_model": "vm-fallback",
            "members": [{"ref": "vm:big", "name": "big", "when": {"chars_gte": 9999}}]
        });
        let body = json!({"messages":[{"role":"user","content":"short"}]});
        let ch = heuristic_choice(&c, &req_metrics(&body)).unwrap();
        assert_eq!(ch.model, "vm-fallback");
        assert_eq!(ch.reason, "heuristic:unmatched→fallback");
    }

    #[test]
    fn metrics_detect_system_and_msgs() {
        let body = json!({"messages":[
            {"role":"system","content":"be terse"},
            {"role":"user","content":"a"},
            {"role":"assistant","content":"b"},
            {"role":"user","content":"SELECT x FROM t"}]});
        let m = req_metrics(&body);
        assert!(m.system && m.msgs == 4 && m.code);
    }
}
