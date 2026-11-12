//! Local scheduler: admission control for priority (local) and tier-2 requests.
//!
//! Slot model (per deployment):
//! - `max_concurrent`: total slots (all classes)
//! - `local_reserve`: slots guaranteed for local priority traffic
//! - `tier2_max`: max concurrent tier-2 slots (client-enforced:
//!   tier2_max <= max_concurrent - local_reserve)
//!
//! Admission rules:
//! - local (priority): admit while `local_active + tier2_active < max_concurrent`.
//!   Local requests may use reserve slots; running tier-2 requests are never
//!   preempted — the reserve only guarantees *new* tier-2 admissions stop.
//! - tier2: admit while `tier2_active < tier2_max` AND
//!   `local_active + tier2_active < max_concurrent - local_reserve` AND
//!   the deployment is `accepting` (sharing enabled, health ok).
//!
//! No local queue: admit or reject immediately.

use loom_core::capability::SharingMode;
use loom_core::config::DeploymentConfig;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    Local,
    Tier2,
}

impl RequestClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestClass::Local => "local",
            RequestClass::Tier2 => "tier2",
        }
    }
}

#[derive(Debug)]
pub enum RejectReason {
    NoCapacity,
    Tier2Disabled,
    BackendDown,
    UnknownDeployment,
    AdmissionClosed,
    /// Request's token footprint would push the deployment over its KV
    /// token budget — the backstop to the server's authoritative gate.
    KvFull,
}

impl RejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::NoCapacity => "no_capacity",
            RejectReason::Tier2Disabled => "tier2_disabled",
            RejectReason::BackendDown => "backend_down",
            RejectReason::UnknownDeployment => "unknown_deployment",
            RejectReason::AdmissionClosed => "admission_closed",
            RejectReason::KvFull => "kv_full",
        }
    }
}

/// Live-adjustable limits for a deployment.
struct DeploymentLimits {
    max_concurrent: AtomicU32,
    local_reserve: AtomicU32,
    tier2_max: AtomicU32,
    sharing_mode: RwLock<SharingMode>,
    enabled: AtomicU32, // 1 = enabled
    /// Local pressure gate: when set, tier2 admissions are refused even if
    /// arithmetic capacity would remain (set on local saturation).
    tier2_gate_closed: AtomicU32,
    /// Backend reachable (set by the health loop). Tier-2 only.
    health_ok: AtomicU32,
    /// Last reported effective health (adapter probe + breaker):
    /// 0=unknown, 1=healthy, 2=degraded, 3=down. Reported to the server so
    /// it can tell a dead backend apart from a merely-busy draining member.
    health: AtomicU32,
    /// Model actually servable: installed and — for auto_load deployments —
    /// resident in the backend (not evicted). Tier-2 only.
    model_ready: AtomicU32,
    /// Adapter-measured KV token capacity (reported to the server as the
    /// deployment's derived budget). 0 = backend cannot be measured.
    kv_budget: std::sync::atomic::AtomicU64,
    /// Server-pushed budget override (desired state). Wins over the
    /// adapter-measured value for admission; never reported back up —
    /// the server already owns that number.
    kv_budget_admin: std::sync::atomic::AtomicU64,
    /// Admission mode pushed by desired state: 0=auto, 1=slots, 2=tokens.
    admission_mode: AtomicU32,
}

impl DeploymentLimits {
    fn from_config(c: &DeploymentConfig) -> Self {
        Self {
            max_concurrent: AtomicU32::new(c.max_concurrent),
            local_reserve: AtomicU32::new(c.local_reserve),
            tier2_max: AtomicU32::new(c.tier2_max),
            sharing_mode: RwLock::new(c.sharing_mode),
            enabled: AtomicU32::new(c.enabled as u32),
            tier2_gate_closed: AtomicU32::new(0),
            // Optimistic until the first health pass (~immediately after
            // startup) proves otherwise — keeps tier-2 usable in contexts
            // without a health loop and during the probe race window.
            health_ok: AtomicU32::new(1),
            health: AtomicU32::new(1),
            model_ready: AtomicU32::new(1),
            kv_budget: std::sync::atomic::AtomicU64::new(0),
            kv_budget_admin: std::sync::atomic::AtomicU64::new(0),
            admission_mode: AtomicU32::new(0),
        }
    }

    /// Enforce the invariant tier2_max <= max_concurrent - local_reserve.
    fn clamp_tier2(&self) {
        let max_c = self.max_concurrent.load(Ordering::SeqCst);
        let reserve = self.local_reserve.load(Ordering::SeqCst);
        let cap = max_c.saturating_sub(reserve);
        let t2 = self.tier2_max.load(Ordering::SeqCst);
        if t2 > cap {
            self.tier2_max.store(cap, Ordering::SeqCst);
        }
    }
}

struct Counters {
    local_active: AtomicU32,
    tier2_active: AtomicU32,
    /// Tier-2 token footprints currently reserved (KV budget accounting).
    kv_used: std::sync::atomic::AtomicU64,
}

pub struct DeploymentSlots {
    pub id: String,
    pub name: String,
    limits: DeploymentLimits,
    counters: Counters,
}

impl DeploymentSlots {
    fn admit(
        &self,
        class: RequestClass,
        sharing_enabled: bool,
        footprint: u64,
    ) -> Result<(), RejectReason> {
        let max_c = self.limits.max_concurrent.load(Ordering::SeqCst);
        let reserve = self.limits.local_reserve.load(Ordering::SeqCst);
        let t2_max = self.limits.tier2_max.load(Ordering::SeqCst);
        let local = self.counters.local_active.load(Ordering::SeqCst);
        let t2 = self.counters.tier2_active.load(Ordering::SeqCst);

        if self.limits.enabled.load(Ordering::SeqCst) == 0 {
            return Err(RejectReason::AdmissionClosed);
        }

        match class {
            RequestClass::Local => {
                if local + t2 >= max_c {
                    return Err(RejectReason::NoCapacity);
                }
                self.counters.local_active.fetch_add(1, Ordering::SeqCst);
            }
            RequestClass::Tier2 => {
                if !sharing_enabled {
                    return Err(RejectReason::Tier2Disabled);
                }
                if *self.limits.sharing_mode.read().unwrap() == SharingMode::Private {
                    return Err(RejectReason::Tier2Disabled);
                }
                if self.limits.tier2_gate_closed.load(Ordering::SeqCst) == 1 {
                    return Err(RejectReason::AdmissionClosed);
                }
                if self.limits.health_ok.load(Ordering::SeqCst) == 0
                    || self.limits.model_ready.load(Ordering::SeqCst) == 0
                {
                    return Err(RejectReason::BackendDown);
                }
                if t2 >= t2_max {
                    return Err(RejectReason::NoCapacity);
                }
                // Reserve stays free for local traffic.
                if local + t2 >= max_c.saturating_sub(reserve) {
                    return Err(RejectReason::NoCapacity);
                }
                // KV token budget backstop: the server's lease gate is
                // authoritative; this mirrors it so a mis-admitted or
                // oversized assignment is still refused. Admin override
                // (desired state) wins over the adapter-measured budget;
                // 0/unknown → slot-only admission.
                let mode = self.limits.admission_mode.load(Ordering::SeqCst);
                if mode != 1 {
                    let budget = {
                        let admin = self.limits.kv_budget_admin.load(Ordering::SeqCst);
                        if admin > 0 {
                            admin
                        } else {
                            self.limits.kv_budget.load(Ordering::SeqCst)
                        }
                    };
                    // 'tokens' mode requires a budget — none configured or
                    // measured means the deployment admits nothing until
                    // one arrives (retryable: desired push may land later).
                    if mode == 2 && budget == 0 {
                        return Err(RejectReason::AdmissionClosed);
                    }
                    if budget > 0 && footprint > 0 {
                        let used = self.counters.kv_used.load(Ordering::SeqCst);
                        if used + footprint > budget {
                            return Err(RejectReason::KvFull);
                        }
                        self.counters.kv_used.fetch_add(footprint, Ordering::SeqCst);
                    }
                }
                self.counters.tier2_active.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    fn release(&self, class: RequestClass, footprint: u64) {
        match class {
            RequestClass::Local => {
                self.counters.local_active.fetch_sub(1, Ordering::SeqCst);
            }
            RequestClass::Tier2 => {
                self.counters.tier2_active.fetch_sub(1, Ordering::SeqCst);
                if footprint > 0 {
                    // Saturating release — a stale/duplicate drop must never
                    // underflow the counter.
                    self.counters
                        .kv_used
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                            Some(v.saturating_sub(footprint))
                        })
                        .ok();
                }
            }
        }
    }

    /// Free tier-2 slots the server may currently book (arithmetic view).
    fn tier2_free(&self, sharing_enabled: bool) -> u32 {
        if !sharing_enabled
            || self.limits.enabled.load(Ordering::SeqCst) == 0
            || *self.limits.sharing_mode.read().unwrap() == SharingMode::Private
            || self.limits.tier2_gate_closed.load(Ordering::SeqCst) == 1
        {
            return 0;
        }
        let max_c = self.limits.max_concurrent.load(Ordering::SeqCst);
        let reserve = self.limits.local_reserve.load(Ordering::SeqCst);
        let t2_max = self.limits.tier2_max.load(Ordering::SeqCst);
        let local = self.counters.local_active.load(Ordering::SeqCst);
        let t2 = self.counters.tier2_active.load(Ordering::SeqCst);

        let shared_cap = max_c.saturating_sub(reserve);
        let room_total = shared_cap.saturating_sub(local + t2);
        let room_t2 = t2_max.saturating_sub(t2);
        room_total.min(room_t2)
    }
}

/// RAII admission token: releases the slot on drop.
pub struct AdmitToken {
    dep: String,
    class: RequestClass,
    sched: LocalScheduler,
    cancel: CancellationToken,
    /// KV tokens this admission reserved — returned to the counter on drop.
    footprint: u64,
    _priv: (),
}

impl AdmitToken {
    pub fn deployment_id(&self) -> &str {
        &self.dep
    }
    pub fn class(&self) -> RequestClass {
        self.class
    }
    /// Cancel-scope for this request (cancelled by connector/admin abort).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

impl Drop for AdmitToken {
    fn drop(&mut self) {
        self.sched.release(&self.dep, self.class, self.footprint);
    }
}

#[derive(Clone)]
pub struct LocalScheduler {
    inner: Arc<Inner>,
}

struct Inner {
    deployments: RwLock<HashMap<String, Arc<DeploymentSlots>>>,
    /// Global switch: workstation participates in sharing.
    sharing_enabled: RwLock<bool>,
    /// Per-request cancel tokens for admin/connector-driven aborts.
    request_cancels: RwLock<HashMap<String, CancellationToken>>,
}

impl LocalScheduler {
    pub fn new(sharing_enabled: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                deployments: RwLock::new(HashMap::new()),
                sharing_enabled: RwLock::new(sharing_enabled),
                request_cancels: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Rebuild deployment slots from config (preserves counters where possible).
    pub fn sync_deployments(&self, cfgs: &[DeploymentConfig]) {
        let mut map = self.inner.deployments.write().unwrap();
        let mut next: HashMap<String, Arc<DeploymentSlots>> = HashMap::new();
        for c in cfgs {
            if let Some(existing) = map.get(&c.id) {
                // update limits in place (counters preserved)
                existing
                    .limits
                    .max_concurrent
                    .store(c.max_concurrent, Ordering::SeqCst);
                existing
                    .limits
                    .local_reserve
                    .store(c.local_reserve, Ordering::SeqCst);
                existing
                    .limits
                    .tier2_max
                    .store(c.tier2_max, Ordering::SeqCst);
                existing.limits.clamp_tier2();
                *existing.limits.sharing_mode.write().unwrap() = c.sharing_mode;
                existing
                    .limits
                    .enabled
                    .store(c.enabled as u32, Ordering::SeqCst);
                next.insert(c.id.clone(), existing.clone());
            } else {
                let limits = DeploymentLimits::from_config(c);
                limits.clamp_tier2();
                next.insert(
                    c.id.clone(),
                    Arc::new(DeploymentSlots {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        limits,
                        counters: Counters {
                            local_active: AtomicU32::new(0),
                            tier2_active: AtomicU32::new(0),
                            kv_used: std::sync::atomic::AtomicU64::new(0),
                        },
                    }),
                );
            }
        }
        *map = next;
    }

    /// Update a single deployment's limits (desired-state application).
    pub fn update_limits(&self, dep_id: &str, dd: &loom_core::config::DesiredDeployment) {
        let map = self.inner.deployments.read().unwrap();
        if let Some(d) = map.get(dep_id) {
            if let Some(v) = dd.local_reserve {
                d.limits.local_reserve.store(v, Ordering::SeqCst);
            }
            if let Some(v) = dd.tier2_max {
                d.limits.tier2_max.store(v, Ordering::SeqCst);
            }
            d.limits.clamp_tier2();
            if let Some(m) = dd.sharing_mode {
                *d.limits.sharing_mode.write().unwrap() = m;
            }
            if let Some(e) = dd.enabled {
                d.limits.enabled.store(e as u32, Ordering::SeqCst);
            }
            if let Some(b) = dd.kv_budget_tokens {
                d.limits.kv_budget_admin.store(b, Ordering::SeqCst);
            }
            if let Some(m) = &dd.admission_mode {
                let v = match m.as_str() {
                    "slots" => 1,
                    "tokens" => 2,
                    _ => 0,
                };
                d.limits.admission_mode.store(v, Ordering::SeqCst);
            }
        }
    }

    /// Adapter-measured KV capacity for a deployment (probe result). This is
    /// the value reported to the server — the admin override stays separate
    /// so clearing it server-side falls back to exactly this number.
    pub fn set_kv_capacity(&self, dep_id: &str, budget: u64) {
        let map = self.inner.deployments.read().unwrap();
        if let Some(d) = map.get(dep_id) {
            d.limits.kv_budget.store(budget, Ordering::SeqCst);
        }
    }

    pub fn set_sharing_enabled(&self, v: bool) {
        *self.inner.sharing_enabled.write().unwrap() = v;
    }

    pub fn sharing_enabled(&self) -> bool {
        *self.inner.sharing_enabled.read().unwrap()
    }

    /// Try to admit a request on a specific deployment. `footprint` is the
    /// server's KV-token reservation for the request (0 = not counted).
    pub fn try_admit(
        &self,
        dep_id: &str,
        class: RequestClass,
        request_id: &str,
        footprint: u64,
    ) -> Result<AdmitToken, RejectReason> {
        let map = self.inner.deployments.read().unwrap();
        let Some(dep) = map.get(dep_id) else {
            return Err(RejectReason::UnknownDeployment);
        };
        let sharing = *self.inner.sharing_enabled.read().unwrap();
        dep.admit(class, sharing, footprint)?;
        let cancel = CancellationToken::new();
        self.inner
            .request_cancels
            .write()
            .unwrap()
            .insert(request_id.to_string(), cancel.clone());
        Ok(AdmitToken {
            dep: dep.id.clone(),
            class,
            sched: self.clone(),
            cancel,
            footprint,
            _priv: (),
        })
    }

    /// Find a deployment that can admit a local request for `model_hint`
    /// (deployment name or model name match). Returns candidates sorted by
    /// current free capacity.
    pub fn local_candidates(
        &self,
        model_hint: &str,
        resolve: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<String> {
        let map = self.inner.deployments.read().unwrap();
        let mut out: Vec<(String, u32)> = Vec::new();
        for (id, dep) in map.iter() {
            if dep.limits.enabled.load(Ordering::SeqCst) == 0 {
                continue;
            }
            if let Some(resolved) = resolve(id) {
                if resolved == model_hint {
                    let max_c = dep.limits.max_concurrent.load(Ordering::SeqCst);
                    let used = dep.counters.local_active.load(Ordering::SeqCst)
                        + dep.counters.tier2_active.load(Ordering::SeqCst);
                    out.push((id.clone(), max_c.saturating_sub(used)));
                }
            }
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.1));
        out.into_iter().map(|(id, _)| id).collect()
    }

    /// Cancel an in-flight request by id (admin abort / server cancel).
    pub fn cancel_request(&self, request_id: &str) -> bool {
        if let Some(tok) = self.inner.request_cancels.read().unwrap().get(request_id) {
            tok.cancel();
            true
        } else {
            false
        }
    }

    /// Forget a finished request's cancel token.
    pub fn finish_request(&self, request_id: &str) {
        self.inner
            .request_cancels
            .write()
            .unwrap()
            .remove(request_id);
    }

    fn release(&self, dep_id: &str, class: RequestClass, footprint: u64) {
        let map = self.inner.deployments.read().unwrap();
        if let Some(dep) = map.get(dep_id) {
            dep.release(class, footprint);
        }
    }

    /// Snapshot of capacity info for all deployments (for server reports + UI).
    pub fn capacity_snapshot(&self) -> Vec<DeploymentCapacityInfo> {
        let sharing = *self.inner.sharing_enabled.read().unwrap();
        let map = self.inner.deployments.read().unwrap();
        map.values()
            .map(|d| {
                let t2_free = d.tier2_free(sharing);
                let local = d.counters.local_active.load(Ordering::SeqCst);
                let t2 = d.counters.tier2_active.load(Ordering::SeqCst);
                DeploymentCapacityInfo {
                    deployment_id: d.id.clone(),
                    tier2_total: if sharing
                        && *d.limits.sharing_mode.read().unwrap() != SharingMode::Private
                        && d.limits.enabled.load(Ordering::SeqCst) == 1
                    {
                        d.limits.tier2_max.load(Ordering::SeqCst)
                    } else {
                        0
                    },
                    tier2_free: t2_free,
                    inflight_local: local,
                    inflight_tier2: t2,
                    sharing_mode: *d.limits.sharing_mode.read().unwrap(),
                    health_ok: d.limits.health_ok.load(Ordering::SeqCst) == 1,
                    health: health_from_u32(d.limits.health.load(Ordering::SeqCst)),
                    model_ready: d.limits.model_ready.load(Ordering::SeqCst) == 1,
                    accepting: t2_free > 0
                        && d.limits.tier2_gate_closed.load(Ordering::SeqCst) == 0
                        && d.limits.health_ok.load(Ordering::SeqCst) == 1
                        && d.limits.model_ready.load(Ordering::SeqCst) == 1,
                    pressure_gated: d.limits.tier2_gate_closed.load(Ordering::SeqCst) == 1,
                    kv_budget_tokens: d.limits.kv_budget.load(Ordering::SeqCst),
                    kv_used_tokens: d.counters.kv_used.load(Ordering::SeqCst),
                }
            })
            .collect()
    }

    /// Close (or reopen) the tier-2 gate on every non-private deployment.
    /// Returns true when any gate state actually changed (→ push capacity).
    pub fn apply_pressure_gate(&self, closed: bool) -> bool {
        let map = self.inner.deployments.read().unwrap();
        let mut changed = false;
        for d in map.values() {
            if *d.limits.sharing_mode.read().unwrap() == SharingMode::Private {
                continue;
            }
            let prev = d
                .limits
                .tier2_gate_closed
                .swap(closed as u32, Ordering::SeqCst);
            changed |= prev != closed as u32;
        }
        changed
    }

    /// Deployments with zero in-flight requests — safe to unload without
    /// disrupting either local or tier-2 work.
    pub fn idle_deployments(&self) -> Vec<String> {
        let map = self.inner.deployments.read().unwrap();
        map.values()
            .filter(|d| {
                d.counters.local_active.load(Ordering::SeqCst) == 0
                    && d.counters.tier2_active.load(Ordering::SeqCst) == 0
            })
            .map(|d| d.id.clone())
            .collect()
    }

    /// Set backend-health / model-readiness reported by the health loop.
    /// `health` is the effective Health (adapter probe + breaker, see
    /// backend_manager::inventory) — health_ok derives from it. Returns true
    /// when the effective flags changed (→ push capacity).
    pub fn set_dep_ready(
        &self,
        dep_id: &str,
        health: crate::adapters::Health,
        model_ready: bool,
    ) -> bool {
        let map = self.inner.deployments.read().unwrap();
        let Some(d) = map.get(dep_id) else {
            return false;
        };
        let (h, hs, m) = (
            (health == crate::adapters::Health::Healthy) as u32,
            health_to_u32(health),
            model_ready as u32,
        );
        let prev_h = d.limits.health_ok.swap(h, Ordering::SeqCst);
        let prev_hs = d.limits.health.swap(hs, Ordering::SeqCst);
        let prev_m = d.limits.model_ready.swap(m, Ordering::SeqCst);
        prev_h != h || prev_hs != hs || prev_m != m
    }

    /// Whether the tier-2 gate is currently closed for a deployment.
    pub fn tier2_gated(&self, dep_id: &str) -> bool {
        let map = self.inner.deployments.read().unwrap();
        map.get(dep_id)
            .map(|d| d.limits.tier2_gate_closed.load(Ordering::SeqCst) == 1)
            .unwrap_or(false)
    }

    /// Open/close the tier-2 admission gate for a deployment (dynamic stop).
    pub fn set_tier2_gate(&self, dep_id: &str, closed: bool) {
        let map = self.inner.deployments.read().unwrap();
        if let Some(d) = map.get(dep_id) {
            d.limits
                .tier2_gate_closed
                .store(closed as u32, Ordering::SeqCst);
        }
    }

    /// Counter snapshot for diagnostics.
    pub fn counters(&self) -> Vec<(String, u32, u32)> {
        let map = self.inner.deployments.read().unwrap();
        map.values()
            .map(|d| {
                (
                    d.id.clone(),
                    d.counters.local_active.load(Ordering::SeqCst),
                    d.counters.tier2_active.load(Ordering::SeqCst),
                )
            })
            .collect()
    }
}

fn health_to_u32(h: crate::adapters::Health) -> u32 {
    match h {
        crate::adapters::Health::Unknown => 0,
        crate::adapters::Health::Healthy => 1,
        crate::adapters::Health::Degraded => 2,
        crate::adapters::Health::Down => 3,
    }
}

fn health_from_u32(v: u32) -> &'static str {
    match v {
        1 => "healthy",
        2 => "degraded",
        3 => "down",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeploymentCapacityInfo {
    pub deployment_id: String,
    pub tier2_total: u32,
    pub tier2_free: u32,
    pub inflight_local: u32,
    pub inflight_tier2: u32,
    pub sharing_mode: SharingMode,
    pub health_ok: bool,
    /// Effective deployment health sent to the server ("healthy"|"degraded"|
    /// "down"|"unknown") — same source the inventory report uses.
    pub health: &'static str,
    pub model_ready: bool,
    pub accepting: bool,
    /// Closed by the system-pressure monitor (cpu/ram over configured limits).
    pub pressure_gated: bool,
    /// Adapter-measured KV token capacity (0 = unknown/unmeasurable).
    pub kv_budget_tokens: u64,
    /// Tier-2 token footprints currently reserved.
    pub kv_used_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::config::{DeploymentConfig, SharingMode};

    fn dep(id: &str) -> DeploymentConfig {
        DeploymentConfig {
            id: id.into(),
            name: id.into(),
            model_name: "m".into(),
            capabilities: Default::default(),
            capability_overrides: false,
            max_concurrent: 4,
            local_reserve: 1,
            tier2_max: 2,
            auto_load: true,
            sharing_mode: SharingMode::Shared,
            enabled: true,
        }
    }

    #[test]
    fn tier2_gated_by_health_and_model_ready() {
        let s = LocalScheduler::new(true);
        s.sync_deployments(&[dep("d1")]);

        // defaults: optimistic → accepting + admit ok
        assert!(s.capacity_snapshot()[0].accepting);
        {
            let _tok = s.try_admit("d1", RequestClass::Tier2, "r0", 0).unwrap();
        } // token drop releases the slot

        // backend down → admit rejected + not accepting + health reports down
        assert!(s.set_dep_ready("d1", crate::adapters::Health::Down, true));
        let snap = &s.capacity_snapshot()[0];
        assert!(!snap.accepting && !snap.health_ok && snap.health == "down");
        assert!(matches!(
            s.try_admit("d1", RequestClass::Tier2, "r1", 0),
            Err(RejectReason::BackendDown)
        ));

        // healthy but model evicted → same gate
        assert!(s.set_dep_ready("d1", crate::adapters::Health::Healthy, false));
        assert_eq!(s.capacity_snapshot()[0].health, "healthy");
        assert!(matches!(
            s.try_admit("d1", RequestClass::Tier2, "r2", 0),
            Err(RejectReason::BackendDown)
        ));
        assert!(!s.capacity_snapshot()[0].accepting);

        // back to ready → accepting again, no more change notifications
        assert!(s.set_dep_ready("d1", crate::adapters::Health::Healthy, true));
        assert!(s.capacity_snapshot()[0].accepting);
        assert!(!s.set_dep_ready("d1", crate::adapters::Health::Healthy, true));

        // local requests are never gated by backend health
        assert!(s.set_dep_ready("d1", crate::adapters::Health::Down, false));
        assert!(s.try_admit("d1", RequestClass::Local, "r3", 0).is_ok());
    }

    #[test]
    fn pressure_gate_closes_t2_never_local_and_idle_detection() {
        let s = LocalScheduler::new(true);
        let mut priv_cfg = dep("priv");
        priv_cfg.sharing_mode = SharingMode::Private;
        s.sync_deployments(&[dep("d1"), priv_cfg]);

        // gate closes only shared deployments, never private or local class
        assert!(s.apply_pressure_gate(true));
        assert!(matches!(
            s.try_admit("d1", RequestClass::Tier2, "r1", 0),
            Err(RejectReason::AdmissionClosed)
        ));
        let snap = s.capacity_snapshot();
        assert!(
            snap.iter()
                .find(|c| c.deployment_id == "d1")
                .unwrap()
                .pressure_gated
        );
        assert!(
            !snap
                .iter()
                .find(|c| c.deployment_id == "priv")
                .unwrap()
                .pressure_gated
        );
        assert!(s.try_admit("d1", RequestClass::Local, "r2", 0).is_ok());
        s.finish_request("r2");

        // in-flight local request keeps the deployment off the idle list
        let _held = s.try_admit("d1", RequestClass::Local, "r3", 0).unwrap();
        assert!(!s.idle_deployments().contains(&"d1".to_string()));
        drop(_held);

        // no change when state already reached; reopen restores admission
        assert!(!s.apply_pressure_gate(true));
        assert!(s.apply_pressure_gate(false));
        assert!(s.try_admit("d1", RequestClass::Tier2, "r4", 0).is_ok());
        let snap = s.capacity_snapshot();
        assert!(
            !snap
                .iter()
                .find(|c| c.deployment_id == "d1")
                .unwrap()
                .pressure_gated
        );
    }
}
