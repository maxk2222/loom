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
    /// Member-configured request overrides merged into the body before
    /// dispatch (`{"set":{"reasoning_effort":"high"}}` — reasoning
    /// escalation: route to a reasoning model AND raise its effort).
    pub set: Value,
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
            // Capability tags (reasoning, vision, …) steer the engine toward
            // the right member better than prose alone.
            let tags = m["tags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let tag_sfx = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{tags}]")
            };
            menu.push_str(&format!("- {name}{tag_sfx}: {desc}\n"));
        }
    }
    let tpl = cfg_str(cfg, "system_prompt");
    let tpl = if tpl.is_empty() { DEFAULT_PROMPT } else { tpl };
    // Both placeholders may appear in the template; {{request}} is also sent
    // as the user message so templates that omit it still work.
    let system = tpl
        .replace("{{models}}", menu.trim_end())
        .replace("{{request}}", excerpt);
    // Identical prompts under an identical config classify once per ~2min —
    // repeated requests skip the engine round-trip entirely. The key covers
    // the whole router config, so member/prompt edits invalidate naturally.
    let cache_key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        system.hash(&mut h);
        excerpt.hash(&mut h);
        h.finish()
    };
    if let Some(hit) = cache_get(cache_key) {
        return Ok(hit);
    }
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
    let out = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !out.is_empty() {
        cache_put(cache_key, out.clone());
    }
    Ok(out)
}

/// Tiny bounded classification cache — engine calls are the expensive part
/// of LLM routing; identical prompts are common (retries, test consoles).
fn class_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<u64, (std::time::Instant, String)>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u64, (std::time::Instant, String)>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn cache_get(key: u64) -> Option<String> {
    let mut g = class_cache().lock().ok()?;
    match g.get(&key) {
        Some((ts, raw)) if ts.elapsed().as_secs() < 120 => Some(raw.clone()),
        _ => {
            g.remove(&key);
            None
        }
    }
}

fn cache_put(key: u64, raw: String) {
    if let Ok(mut g) = class_cache().lock() {
        if g.len() >= 256 {
            g.retain(|_, (ts, _)| ts.elapsed().as_secs() < 120);
            if g.len() >= 256 {
                g.clear(); // still full: drop rather than grow unbounded
            }
        }
        g.insert(key, (std::time::Instant::now(), raw));
    }
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
    // Containment on word boundaries (longest member name first so
    // `fast-code` wins over `fast`) — raw substring matching misfires on
    // noise like "breakfast" containing "fast".
    let mut by_len: Vec<&Value> = members.iter().collect();
    by_len.sort_by_key(|m| std::cmp::Reverse(m["name"].as_str().unwrap_or("").len()));
    for m in by_len {
        let n = norm(m["name"].as_str().unwrap_or(""));
        if !n.is_empty() && contains_word(&raw_n, &n) {
            return Some(to_choice(m, raw, "contains"));
        }
    }
    None
}

/// Whole-word containment: `needle` appears in `hay` bounded by
/// non-alphanumeric characters on both sides.
fn contains_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        let before_ok = hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = hay[i + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        before_ok && after_ok
    })
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
        set: m["set"]
            .as_object()
            .map(|o| Value::Object(o.clone()))
            .unwrap_or_default(),
    }
}

/// Request signals used by `mode: "heuristic"` routers — cheap and fully
/// deterministic, no engine call needed.
#[derive(Debug, Default)]
pub struct ReqMetrics {
    /// Chars of the last user message text.
    pub chars: usize,
    /// Rough token estimate of the whole request text (chars/4) — rules can
    /// think in model-facing units instead of raw chars.
    pub est_tokens: usize,
    /// Message count.
    pub msgs: usize,
    /// Code fences / language hints in the request text.
    pub code: bool,
    /// Multimodal image parts present.
    pub images: bool,
    /// Audio parts (`input_audio`/`audio_url`) present.
    pub audio: bool,
    /// A system message is present.
    pub system: bool,
    /// `tools`/`functions` declared — agentic request.
    pub tools: bool,
    /// `response_format` asks for JSON output.
    pub json_mode: bool,
    /// Caller-requested reasoning effort: `reasoning_effort` or
    /// `reasoning.effort` mapped none..max → 1..=7, 0 = unset.
    pub effort: u8,
    /// Reasoning intent detected in the text (step-by-step / proof / math
    /// cues) — the deterministic half of reasoning escalation.
    pub reasoning: bool,
}

/// Compute request metrics from the raw OpenAI body.
pub fn req_metrics(body: &Value) -> ReqMetrics {
    let mut m = ReqMetrics {
        effort: effort_level(body),
        tools: body["tools"].as_array().is_some_and(|t| !t.is_empty())
            || body["functions"].as_array().is_some_and(|t| !t.is_empty()),
        json_mode: matches!(
            body["response_format"]["type"].as_str(),
            Some("json_object") | Some("json_schema")
        ),
        chars: user_excerpt(body, usize::MAX).chars().count(),
        ..Default::default()
    };
    let mut all_text = String::new();
    if let Some(ms) = body["messages"].as_array() {
        m.msgs = ms.len();
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
                            Some("input_audio") | Some("audio_url") | Some("audio") => {
                                m.audio = true
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            all_text.push('\n');
        }
    }
    // /v1/completions style: single prompt field
    if m.msgs == 0 {
        if let Some(p) = body["prompt"].as_str() {
            all_text.push_str(p);
            m.chars = p.chars().count();
        }
    }
    m.code = looks_like_code(&all_text);
    m.reasoning = looks_like_reasoning(&all_text);
    m.est_tokens = all_text.chars().count() / 4;
    m
}

/// Map `reasoning_effort` / `reasoning.effort` to 0..=6 — both the Chat
/// Completions flat form and the Responses-style object are accepted.
fn effort_level(body: &Value) -> u8 {
    let raw = body["reasoning_effort"]
        .as_str()
        .or_else(|| body["reasoning"]["effort"].as_str())
        .unwrap_or("");
    match raw.to_ascii_lowercase().as_str() {
        "none" => 1,
        "minimal" => 2,
        "low" => 3,
        "medium" => 4,
        "high" => 5,
        "xhigh" => 6,
        "max" => 7,
        _ => 0,
    }
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

/// Heuristic reasoning-intent cues — false positives merely route to a
/// stronger model (the safe direction). Covers EN + DE phrasing.
fn looks_like_reasoning(t: &str) -> bool {
    let t = t.to_lowercase();
    [
        "step by step",
        "step-by-step",
        "think hard",
        "think carefully",
        "think deeply",
        "reason about",
        "chain of thought",
        "chain-of-thought",
        "prove ",
        "proof that",
        "derive ",
        "analyze in depth",
        "deep dive",
        "schritt für schritt",
        "beweise",
        "leite her",
        "denke genau nach",
    ]
    .iter()
    .any(|k| t.contains(k))
}

fn metrics_summary(m: &ReqMetrics) -> String {
    format!(
        "chars={} tokens~={} msgs={} code={} images={} audio={} system={} tools={} json_mode={} effort={} reasoning={}",
        m.chars, m.est_tokens, m.msgs, m.code, m.images, m.audio, m.system,
        m.tools, m.json_mode, m.effort, m.reasoning
    )
}

/// Evaluate a member `when` rule against request metrics — every listed key
/// must match (AND); an empty/absent rule is the catch-all member.
/// Combinators: `"any": [sub-rules]` = OR, `"not": {sub-rule}` = negation.
fn when_matches(when: &Value, m: &ReqMetrics) -> bool {
    let Some(o) = when.as_object() else {
        return when.is_null();
    };
    for (k, v) in o {
        let ok = match k.as_str() {
            "chars_lt" => v.as_u64().is_some_and(|n| (m.chars as u64) < n),
            "chars_gte" => v.as_u64().is_some_and(|n| (m.chars as u64) >= n),
            "tokens_lt" => v.as_u64().is_some_and(|n| (m.est_tokens as u64) < n),
            "tokens_gte" => v.as_u64().is_some_and(|n| (m.est_tokens as u64) >= n),
            "msgs_lt" => v.as_u64().is_some_and(|n| (m.msgs as u64) < n),
            "msgs_gte" => v.as_u64().is_some_and(|n| (m.msgs as u64) >= n),
            "code" => v.as_bool().is_some_and(|b| m.code == b),
            "images" => v.as_bool().is_some_and(|b| m.images == b),
            "audio" => v.as_bool().is_some_and(|b| m.audio == b),
            "system" => v.as_bool().is_some_and(|b| m.system == b),
            "tools" => v.as_bool().is_some_and(|b| m.tools == b),
            "json_mode" => v.as_bool().is_some_and(|b| m.json_mode == b),
            "reasoning" => v.as_bool().is_some_and(|b| m.reasoning == b),
            "effort_gte" => effort_at_least(m.effort, v),
            // Unset effort (0) counts as lowest — matches any positive bound.
            "effort_lt" => effort_level_of(v).is_some_and(|n| (m.effort as u64) < n),
            "any" => v
                .as_array()
                .is_some_and(|subs| subs.iter().any(|s| when_matches(s, m))),
            "not" => !when_matches(v, m),
            _ => false, // unknown rule keys never match
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Parse a rule value as effort level — number or name ("high" → 5).
fn effort_level_of(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        v.as_str().and_then(|s| {
            let n = effort_level(&serde_json::json!({"reasoning_effort": s}));
            (n > 0).then_some(n as u64)
        })
    })
}

/// `effort_gte`: an unset request effort never satisfies a positive floor.
fn effort_at_least(effort: u8, v: &Value) -> bool {
    effort_level_of(v).is_some_and(|n| effort > 0 && (effort as u64) >= n)
}

/// Merge member `set` overrides into the request body (gateway, pre-dispatch).
/// `reasoning_effort` is special-cased: the lowest escalation tier disables
/// reasoning, and "off" must stick regardless of which request shape the
/// caller used — flat `reasoning_effort` or Responses-style
/// `reasoning.effort`. Both fields are set so an object-form override cannot
/// silently re-enable reasoning against a flat "none".
/// `model`/`messages`/`stream` can never be hijacked by member config.
pub fn merge_set_overrides(body: &mut Value, set: &Value) {
    let (Some(b), Some(over)) = (body.as_object_mut(), set.as_object()) else {
        return;
    };
    for (k, v) in over {
        if matches!(k.as_str(), "model" | "messages" | "stream") {
            continue;
        }
        b.insert(k.clone(), v.clone());
        if k == "reasoning_effort" {
            let effort = v.clone();
            match b.get_mut("reasoning") {
                Some(Value::Object(o)) => {
                    o.insert("effort".into(), effort);
                }
                Some(slot) => *slot = json!({"effort": effort}),
                None => {
                    b.insert("reasoning".into(), json!({"effort": effort}));
                }
            }
        }
    }
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
            set: Value::Null,
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
                    set: Value::Null,
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
                    set: Value::Null,
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
                "pick": {"model": ch.model, "name": ch.name, "reason": ch.reason, "set": ch.set},
            }),
            Err(e) => json!({"ok": false, "error": e.to_string(), "raw": metrics_summary(&m)}),
        };
    }
    match classify(sched, models, cfg, prompt, 1, deadline_ms).await {
        Ok(text) => json!({
            "ok": true,
            "raw": text,
            "pick": pick_member(cfg, &text)
                .map(|c| json!({"model": c.model, "name": c.name, "reason": c.reason, "set": c.set})),
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

    #[test]
    fn metrics_extract_effort_and_intent() {
        // Chat-Completions flat form + reasoning-intent keywords
        let body = json!({
            "reasoning_effort": "high",
            "messages":[{"role":"user","content":"prove that sqrt(2) is irrational, step by step"}],
        });
        let m = req_metrics(&body);
        assert_eq!(m.effort, 5);
        assert!(m.reasoning);
        // Responses-style object form
        let body2 = json!({
            "reasoning": {"effort": "low"},
            "messages":[{"role":"user","content":"hi"}],
        });
        assert_eq!(req_metrics(&body2).effort, 3);
        assert!(!req_metrics(&body2).reasoning);
    }

    #[test]
    fn metrics_tools_json_mode_audio() {
        let body = json!({
            "tools": [{"type":"function","function":{"name":"x"}}],
            "response_format": {"type":"json_schema"},
            "messages":[{"role":"user","content":[
                {"type":"text","text":"transcribe"},
                {"type":"input_audio","input_audio":{"data":"xx","format":"wav"}},
            ]}],
        });
        let m = req_metrics(&body);
        assert!(m.tools && m.json_mode && m.audio && !m.images);
    }

    #[test]
    fn effort_rules_and_escalation() {
        let c = json!({
            "mode": "heuristic",
            "members": [
                {"ref":"vm:deep","name":"deep","when":{"effort_gte":"high"},
                 "set":{"reasoning_effort":"high"}},
                {"ref":"vm:fast","name":"fast","when":{}}
            ]
        });
        let hi = json!({"reasoning_effort":"high","messages":[{"role":"user","content":"x"}]});
        let ch = heuristic_choice(&c, &req_metrics(&hi)).unwrap();
        assert_eq!(ch.model, "deep");
        assert_eq!(ch.set["reasoning_effort"], "high");
        // Unset/low effort → catch-all
        let lo = json!({"reasoning_effort":"low","messages":[{"role":"user","content":"x"}]});
        assert_eq!(
            heuristic_choice(&c, &req_metrics(&lo)).unwrap().model,
            "fast"
        );
        let none = json!({"messages":[{"role":"user","content":"x"}]});
        assert_eq!(
            heuristic_choice(&c, &req_metrics(&none)).unwrap().model,
            "fast"
        );
    }

    #[test]
    fn reasoning_intent_routes_to_reasoner() {
        let c = json!({
            "mode": "heuristic",
            "members": [
                {"ref":"vm:qwq","name":"reasoner","when":{"reasoning":true}},
                {"ref":"vm:fast","name":"fast","when":{}}
            ]
        });
        let deep = json!({"messages":[{"role":"user","content":"think carefully and derive the formula"}]});
        assert_eq!(
            heuristic_choice(&c, &req_metrics(&deep)).unwrap().model,
            "qwq"
        );
        let shallow = json!({"messages":[{"role":"user","content":"what time is it"}]});
        assert_eq!(
            heuristic_choice(&c, &req_metrics(&shallow)).unwrap().model,
            "fast"
        );
    }

    #[test]
    fn any_and_not_combinators() {
        let c = json!({
            "mode": "heuristic",
            "members": [
                {"ref":"vm:a","name":"a","when":{"any":[{"code":true},{"tokens_gte":2000}]}},
                {"ref":"vm:b","name":"b","when":{"images":true,"not":{"code":true}}},
                {"ref":"vm:z","name":"z","when":{}}
            ]
        });
        let code = json!({"messages":[{"role":"user","content":"```py\nx=1\n```"}]});
        assert_eq!(
            heuristic_choice(&c, &req_metrics(&code)).unwrap().model,
            "a"
        );
        // image without code → b; image WITH code → not excludes → a wins first anyway
        let img = json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"u"}}]}]});
        assert_eq!(heuristic_choice(&c, &req_metrics(&img)).unwrap().model, "b");
    }

    #[test]
    fn word_boundary_avoids_false_contains() {
        let c = cfg();
        // "breakfast" contains "fast" but is not a word match → qwen (exact)
        // still wins only when its own name matches.
        let ch = pick_member(&c, "I ate breakfast today");
        assert!(ch.is_none());
        let ch = pick_member(&c, "choose fast please").unwrap();
        assert_eq!(ch.model, "fast");
    }

    #[test]
    fn merge_set_off_sticks_through_both_forms() {
        // Caller used the object form with high — a flat "none" set must
        // neutralize it so reasoning cannot stay enabled.
        let mut body = json!({
            "model": "x",
            "reasoning": {"effort": "high", "summary": "auto"},
            "messages": [],
        });
        merge_set_overrides(&mut body, &json!({"reasoning_effort": "none"}));
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["reasoning"]["summary"], "auto"); // untouched keys kept
                                                          // Object form created when absent; scalar `reasoning` replaced.
        let mut b2 = json!({"model": "x"});
        merge_set_overrides(
            &mut b2,
            &json!({"reasoning_effort": "high", "temperature": 0.2}),
        );
        assert_eq!(b2["reasoning"]["effort"], "high");
        assert_eq!(b2["temperature"], 0.2);
        let mut b3 = json!({"reasoning": "garbage"});
        merge_set_overrides(&mut b3, &json!({"reasoning_effort": "none"}));
        assert_eq!(b3["reasoning"]["effort"], "none");
        // Hijack keys are never merged.
        let mut b4 = json!({"model": "a", "messages": [1], "stream": false});
        merge_set_overrides(
            &mut b4,
            &json!({"model": "b", "messages": [], "stream": true}),
        );
        assert_eq!(b4["model"], "a");
        assert_eq!(b4["stream"], false);
    }

    #[test]
    fn member_set_carried_into_choice() {
        let c = json!({
            "mode":"heuristic",
            "members":[{"ref":"vm:m","name":"m","when":{},
                        "set":{"temperature":0.1,"reasoning_effort":"max"}}]
        });
        let body = json!({"messages":[{"role":"user","content":"x"}]});
        let ch = heuristic_choice(&c, &req_metrics(&body)).unwrap();
        assert_eq!(ch.set["temperature"], 0.1);
        assert_eq!(ch.set["reasoning_effort"], "max");
    }
}
