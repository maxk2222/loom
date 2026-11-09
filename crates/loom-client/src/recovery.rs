//! Per-deployment circuit breaker + self-healing recovery.
//!
//! A backend can report its model `loaded` and healthy while the generation
//! pipeline is dead (observed: LM Studio's MLX engine crashing on every
//! request after a poisoned vision batch). Health probes cannot see that —
//! only request outcomes can. This module counts consecutive generation
//! failures per deployment and, past a threshold, opens a circuit: the
//! deployment reports `down`, stops admitting tier-2 work, and a recovery
//! task attempts `unload → warm_up` (a real JIT reload verified by an
//! actual generation) with backoff until it heals or lands in quarantine.

use crate::adapters::AdapterError;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Breaker {
    /// Serving normally.
    #[default]
    Closed,
    /// Tripped — excluded from routing while recovery runs.
    Open,
    /// Reload attempts exhausted; stays down and is retried on a long
    /// cadence so a manually repaired backend heals without a client
    /// restart.
    Quarantined,
}

impl Breaker {
    pub fn as_str(&self) -> &'static str {
        match self {
            Breaker::Closed => "closed",
            Breaker::Open => "open",
            Breaker::Quarantined => "quarantined",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DepState {
    pub breaker: Breaker,
    /// Consecutive crash-classified failures since the last success.
    pub consec_failures: u32,
    /// Reload attempts spent on the current incident.
    pub attempts: u32,
    /// Recovery may not run before this instant (backoff between retries).
    pub cooldown_until: Option<Instant>,
    /// A recovery task already owns this deployment (single-flight).
    pub recovering: bool,
}

pub use loom_core::config::RecoveryConfig;

/// Whether a forward/stream error counts as a generation crash toward the
/// breaker. 4xx (bad request, context overflow), cancellations, and adapter
/// protocol errors are not backend faults.
pub fn counts_as_crash(e: &AdapterError) -> bool {
    match e {
        AdapterError::Upstream { status, .. } => *status >= 500,
        AdapterError::Connect(_) | AdapterError::Timeout | AdapterError::Io(_) => true,
        _ => false,
    }
}

pub struct FailureTracker {
    cfg: Mutex<RecoveryConfig>,
    deps: Mutex<HashMap<String, DepState>>,
}

impl FailureTracker {
    pub fn new(cfg: RecoveryConfig) -> Self {
        Self {
            cfg: Mutex::new(cfg),
            deps: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_config(&self, cfg: RecoveryConfig) {
        *self.cfg.lock().unwrap() = cfg;
    }

    pub fn config(&self) -> RecoveryConfig {
        self.cfg.lock().unwrap().clone()
    }

    /// Record a crash-classified failure. Returns true when the breaker
    /// just transitioned to `Open` — the caller should kick recovery.
    pub fn record_failure(&self, dep_id: &str) -> bool {
        let cfg = self.cfg.lock().unwrap();
        if !cfg.enabled {
            return false;
        }
        let threshold = cfg.failure_threshold.max(1);
        drop(cfg);
        let mut deps = self.deps.lock().unwrap();
        let st = deps.entry(dep_id.to_string()).or_default();
        st.consec_failures += 1;
        if st.breaker == Breaker::Closed && st.consec_failures >= threshold {
            st.breaker = Breaker::Open;
            st.attempts = 0;
            st.cooldown_until = None;
            return true;
        }
        false
    }

    /// A clean request completion resets the consecutive counter — but does
    /// NOT close an open breaker: only a verified reload may do that, or a
    /// single lucky success between crashes would flap routing.
    pub fn record_success(&self, dep_id: &str) {
        let mut deps = self.deps.lock().unwrap();
        if let Some(st) = deps.get_mut(dep_id) {
            st.consec_failures = 0;
        }
    }

    /// Recovery verified the deployment serves again — full reset.
    pub fn healed(&self, dep_id: &str) {
        let mut deps = self.deps.lock().unwrap();
        deps.insert(dep_id.to_string(), DepState::default());
    }

    /// Claim the recovery slot (single-flight per deployment). The caller
    /// must release via `finish_recovery`.
    pub fn claim_recovery(&self, dep_id: &str) -> bool {
        let mut deps = self.deps.lock().unwrap();
        let st = deps.entry(dep_id.to_string()).or_default();
        if st.recovering {
            return false;
        }
        st.recovering = true;
        true
    }

    /// Recovery task exiting (healed or not) — clears the single-flight flag.
    pub fn finish_recovery(&self, dep_id: &str) {
        let mut deps = self.deps.lock().unwrap();
        if let Some(st) = deps.get_mut(dep_id) {
            st.recovering = false;
        }
    }

    /// After a failed reload attempt: advance the attempt counter and arm
    /// the cooldown. Returns the breaker state to keep (Open while attempts
    /// remain, Quarantined once the budget is spent).
    pub fn attempt_failed(&self, dep_id: &str) -> Breaker {
        let cfg = self.cfg.lock().unwrap().clone();
        let mut deps = self.deps.lock().unwrap();
        let st = deps.entry(dep_id.to_string()).or_default();
        st.attempts += 1;
        let wait = if st.attempts > cfg.max_attempts {
            st.breaker = Breaker::Quarantined;
            cfg.quarantine_retry_secs
        } else {
            let idx = (st.attempts.saturating_sub(1) as usize)
                .min(cfg.backoff_secs.len().saturating_sub(1));
            cfg.backoff_secs.get(idx).copied().unwrap_or(60)
        };
        st.cooldown_until = Some(Instant::now() + Duration::from_secs(wait));
        st.breaker
    }

    /// True while a retry is still cooling down.
    pub fn cooling_down(&self, dep_id: &str) -> bool {
        let deps = self.deps.lock().unwrap();
        deps.get(dep_id)
            .and_then(|st| st.cooldown_until)
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    /// True when the deployment must report `down` to the server — the
    /// routing exclusion signal.
    pub fn reports_down(&self, dep_id: &str) -> bool {
        let deps = self.deps.lock().unwrap();
        deps.get(dep_id)
            .map(|st| matches!(st.breaker, Breaker::Open | Breaker::Quarantined))
            .unwrap_or(false)
    }

    /// Snapshot for the local API status endpoint.
    pub fn state(&self, dep_id: &str) -> DepState {
        let deps = self.deps.lock().unwrap();
        deps.get(dep_id).copied().unwrap_or_default()
    }

    /// All tracked deployment states (status/debug view).
    pub fn states(&self) -> HashMap<String, DepState> {
        self.deps.lock().unwrap().clone()
    }

    /// Drop tracking for deployments removed by config sync.
    pub fn retain(&self, keep: &HashSet<String>) {
        self.deps.lock().unwrap().retain(|id, _| keep.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_5xx_and_transport_count() {
        assert!(counts_as_crash(&AdapterError::Upstream {
            status: 500,
            message: "boom".into()
        }));
        assert!(counts_as_crash(&AdapterError::Connect("x".into())));
        assert!(counts_as_crash(&AdapterError::Timeout));
        assert!(counts_as_crash(&AdapterError::Io("x".into())));
    }

    #[test]
    fn request_level_errors_do_not_count() {
        assert!(!counts_as_crash(&AdapterError::Upstream {
            status: 400,
            message: "bad request".into()
        }));
        assert!(!counts_as_crash(&AdapterError::Upstream {
            status: 429,
            message: "rate".into()
        }));
        assert!(!counts_as_crash(&AdapterError::Cancelled));
        assert!(!counts_as_crash(&AdapterError::Protocol("x".into())));
    }

    #[test]
    fn breaker_opens_at_threshold_and_reports_down() {
        let t = FailureTracker::new(RecoveryConfig {
            failure_threshold: 3,
            ..Default::default()
        });
        assert!(!t.record_failure("d"));
        assert!(!t.record_failure("d"));
        assert!(t.record_failure("d")); // third → opened
        assert!(t.reports_down("d"));
        // Further failures while open do not re-trigger.
        assert!(!t.record_failure("d"));
    }

    #[test]
    fn success_resets_counter_but_not_open_breaker() {
        let t = FailureTracker::new(RecoveryConfig {
            failure_threshold: 2,
            ..Default::default()
        });
        t.record_failure("d");
        t.record_success("d");
        assert!(!t.record_failure("d")); // counter restarted → only 1
        assert!(t.record_failure("d"));
        assert!(t.reports_down("d"));
        t.record_success("d");
        // Open stays open — only healed() closes it.
        assert!(t.reports_down("d"));
    }

    #[test]
    fn attempts_escalate_to_quarantine() {
        let t = FailureTracker::new(RecoveryConfig {
            failure_threshold: 1,
            max_attempts: 2,
            quarantine_retry_secs: 10,
            backoff_secs: vec![1, 2],
            ..Default::default()
        });
        assert!(t.record_failure("d"));
        assert!(t.claim_recovery("d"));
        // claim is single-flight
        assert!(!t.claim_recovery("d"));
        assert_eq!(t.attempt_failed("d"), Breaker::Open);
        assert!(t.cooling_down("d"));
        assert_eq!(t.attempt_failed("d"), Breaker::Open);
        assert_eq!(t.attempt_failed("d"), Breaker::Quarantined);
        t.finish_recovery("d");
        assert!(t.claim_recovery("d")); // re-claimable for the retry cadence
        assert!(t.reports_down("d"));
        t.healed("d");
        assert!(!t.reports_down("d"));
        assert_eq!(t.state("d").attempts, 0);
    }

    #[test]
    fn disabled_tracker_never_trips() {
        let t = FailureTracker::new(RecoveryConfig {
            enabled: false,
            ..Default::default()
        });
        for _ in 0..10 {
            assert!(!t.record_failure("d"));
        }
        assert!(!t.reports_down("d"));
    }

    #[test]
    fn retain_drops_removed_deployments() {
        let t = FailureTracker::new(RecoveryConfig::default());
        t.record_failure("d1");
        t.record_failure("d2");
        t.retain(&HashSet::from(["d1".to_string()]));
        assert_eq!(t.state("d1").consec_failures, 1);
        assert_eq!(t.state("d2").consec_failures, 0);
    }
}
