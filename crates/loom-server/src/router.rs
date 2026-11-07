//! Routing engine: strategy-pluggable candidate ranking with routing trace.

use crate::model_registry::Member;
use loom_core::RequiredCaps;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub ranked: Vec<Member>,
    pub trace: serde_json::Value,
}

pub struct Router {
    rr_counters: HashMap<String, AtomicUsize>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
    Priority,
    LeastInFlight,
    Weighted,
    RoundRobin,
    Manual,
}

impl Strategy {
    pub fn parse(s: &str) -> Self {
        match s {
            "least_in_flight" => Strategy::LeastInFlight,
            "weighted" => Strategy::Weighted,
            "round_robin" => Strategy::RoundRobin,
            "manual" => Strategy::Manual,
            _ => Strategy::Priority,
        }
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            rr_counters: HashMap::new(),
        }
    }

    /// Rank candidates for a virtual model.
    /// `runtime`: deployment_id -> (tier2_free, state) from Redis.
    pub fn route(
        &mut self,
        vm_api_name: &str,
        strategy: Strategy,
        mut candidates: Vec<Member>,
        runtime: &HashMap<String, (u32, String)>,
        required: &RequiredCaps,
    ) -> RouteDecision {
        // Filter: runtime availability + caps already filtered upstream.
        let before = candidates.len();
        candidates.retain(|m| {
            runtime
                .get(&m.deployment_id)
                .map(|(free, st)| *free > 0 && st == "available")
                .unwrap_or(false)
        });
        let filtered_out = before - candidates.len();

        let mut trace = serde_json::json!({
            "strategy": format!("{strategy:?}"),
            "candidates_in": before,
            "filtered_out": filtered_out,
        });

        match strategy {
            Strategy::Priority => {
                candidates.sort_by_key(|m| m.priority);
            }
            Strategy::LeastInFlight => {
                candidates.sort_by_key(|m| {
                    let free = runtime.get(&m.deployment_id).map(|v| v.0).unwrap_or(0);
                    // most free first => ascending negative; use total-free rank
                    (std::cmp::Reverse(free), m.priority)
                });
            }
            Strategy::Weighted => {
                // weighted shuffle approximation: sort by weight desc then stable
                candidates
                    .sort_by(|a, b| b.weight.cmp(&a.weight).then(a.priority.cmp(&b.priority)));
            }
            Strategy::RoundRobin => {
                candidates.sort_by_key(|m| m.priority);
                let ctr = self
                    .rr_counters
                    .entry(vm_api_name.to_string())
                    .or_insert_with(|| AtomicUsize::new(0))
                    .fetch_add(1, Ordering::Relaxed);
                if !candidates.is_empty() {
                    let n = candidates.len();
                    candidates.rotate_left(ctr % n);
                }
            }
            Strategy::Manual => {
                // manual_pinned members first (exactly one expected).
                candidates.sort_by_key(|m| (!m.manual_pinned, m.priority));
            }
        }
        if let Some(top) = candidates.first() {
            trace["selected"] = serde_json::json!(top.deployment_id);
        }
        trace["required_caps"] = serde_json::json!({
            "tools": required.tools, "vision": required.vision,
            "streaming": required.streaming, "json_mode": required.json_mode,
        });
        RouteDecision {
            ranked: candidates,
            trace,
        }
    }
}
