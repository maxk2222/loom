//! Backend manager: owns adapters, runs health checks + capability probing,
//! resolves deployments to backends, exposes inventory for reports.

use crate::adapters::{make_adapter, AdapterError, BackendAdapter, Health};
use loom_core::capability::{ApiType, BackendType, Capabilities};
use loom_core::config::{BackendConfig, ClientConfig, DeploymentConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct DeploymentInfo {
    pub cfg: DeploymentConfig,
    pub backend_id: String,
    /// None = not yet probed (treated permissively).
    pub capabilities: Option<Capabilities>,
    pub health: Health,
}

#[derive(Clone)]
pub struct BackendManager {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    backends: RwLock<HashMap<String, BackendEntry>>,
    deployments: RwLock<HashMap<String, DeploymentEntry>>,
    sys_stats: RwLock<Option<crate::sysmon::SharedStats>>,
    cancel: CancellationToken,
}

struct BackendEntry {
    cfg: BackendConfig,
    adapter: Box<dyn BackendAdapter>,
    health: Arc<RwLock<Health>>,
    models: Arc<RwLock<Vec<String>>>,
    /// Currently-resident models (`loaded_models()`): `Some` = known set
    /// (possibly empty), `None` = backend exposes no loaded-set concept.
    loaded: Arc<RwLock<Option<Vec<String>>>>,
    /// `auto`-binding candidates (`bindable_models()`): resident models
    /// minus kinds that cannot serve chat (e.g. LM Studio embeddings).
    bindable: Arc<RwLock<Option<Vec<String>>>>,
}

struct DeploymentEntry {
    cfg: DeploymentConfig,
    backend_id: String,
    caps: Arc<RwLock<Option<Capabilities>>>,
    health: Arc<RwLock<Health>>,
    /// Last model resolved for a `model_name == "auto"` deployment — a
    /// change invalidates probed caps and refreshes the inventory report.
    resolved_model: Arc<RwLock<Option<String>>>,
    /// Fires the "multiple loaded models" warning once per deployment.
    warned_multi: Arc<std::sync::atomic::AtomicBool>,
    warmed_at: Arc<RwLock<Option<std::time::Instant>>>,
    /// Eviction events where a previously-warmed model went absent from the
    /// loaded set — i.e. it loaded then got evicted (LM Studio LRU under
    /// memory pressure). Backs the re-warm cadence off exponentially instead
    /// of hammering a model that cannot stay resident.
    evict_misses: Arc<std::sync::atomic::AtomicU32>,
    /// True while the current absent streak already counted its miss — a
    /// persistently-evicted model must count once per eviction event, not
    /// once per probe tick, or the backoff hits the 30min cap in ~75s.
    miss_counted: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for BackendManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendManager {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(8)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            inner: Arc::new(Inner {
                http,
                backends: RwLock::new(HashMap::new()),
                deployments: RwLock::new(HashMap::new()),
                sys_stats: RwLock::new(None),
                cancel: CancellationToken::new(),
            }),
        }
    }

    /// Wire the sysmon stats handle so auto-load can check GPU headroom.
    pub async fn set_sys_stats(&self, s: crate::sysmon::SharedStats) {
        *self.inner.sys_stats.write().await = Some(s);
    }

    /// True while GPU VRAM is over the configured `gpu_vram_pct` limit —
    /// warming a model that cannot fit wastes a load cycle. Only counts
    /// when a real VRAM total exists (dedicated GPUs); unified-memory
    /// platforms are covered by the RAM gate instead.
    async fn vram_blocked(&self) -> bool {
        let Some(arc) = self.inner.sys_stats.read().await.clone() else {
            return false;
        };
        let s = arc.read().unwrap().clone();
        let limit = s.gpu_vram_limit_pct;
        if limit == 0 {
            return false;
        }
        let Some(g) = s.gpu else { return false };
        let (Some(total), Some(used)) = (g.mem_total_bytes, g.mem_used_bytes) else {
            return false;
        };
        total > 0 && (used as f32 * 100.0 / total as f32) >= limit as f32
    }

    /// Rebuild backend/deployment registry from config. Preserves probed
    /// capabilities for unchanged deployments.
    pub async fn sync_from_config(&self, cfg: &ClientConfig) {
        let mut backends = self.inner.backends.write().await;
        let mut deployments = self.inner.deployments.write().await;

        let mut next_backends: HashMap<String, BackendEntry> = HashMap::new();
        let mut next_deployments: HashMap<String, DeploymentEntry> = HashMap::new();

        for b in &cfg.backends {
            let adapter = if let Some(existing) = backends.remove(&b.id) {
                if existing.cfg == *b {
                    existing.adapter
                } else {
                    make_adapter(b, self.inner.http.clone())
                }
            } else {
                make_adapter(b, self.inner.http.clone())
            };
            let health = Arc::new(RwLock::new(Health::Unknown));
            let models = Arc::new(RwLock::new(Vec::new()));
            next_backends.insert(
                b.id.clone(),
                BackendEntry {
                    cfg: b.clone(),
                    adapter,
                    health,
                    models,
                    loaded: Arc::new(RwLock::new(None)),
                    bindable: Arc::new(RwLock::new(None)),
                },
            );

            for d in &b.deployments {
                // Carry resolved/caps state forward: a config save must not
                // drop the dynamic binding (would force a spurious caps
                // re-probe + inventory push on the next pass).
                let old = deployments.remove(&d.id);
                let caps = old
                    .as_ref()
                    .map(|e| e.caps.clone())
                    .unwrap_or_else(|| Arc::new(RwLock::new(None)));
                let resolved_model = old
                    .as_ref()
                    .filter(|e| e.backend_id == b.id)
                    .map(|e| e.resolved_model.clone())
                    .unwrap_or_else(|| Arc::new(RwLock::new(None)));
                let warned_multi = old
                    .as_ref()
                    .map(|e| e.warned_multi.clone())
                    .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
                next_deployments.insert(
                    d.id.clone(),
                    DeploymentEntry {
                        resolved_model,
                        warned_multi,
                        warmed_at: Arc::new(RwLock::new(None)),
                        evict_misses: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                        miss_counted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        cfg: d.clone(),
                        backend_id: b.id.clone(),
                        caps,
                        health: Arc::new(RwLock::new(Health::Unknown)),
                    },
                );
            }
        }
        *backends = next_backends;
        *deployments = next_deployments;
    }

    /// Start periodic health checks + capability probing for all backends.
    /// Also computes per-deployment tier-2 readiness (backend reachable +
    /// model installed/loaded) and pushes a capacity report on any change so
    /// the server stops routing to stale deployments quickly.
    /// `inventory_notify` fires when probe results (capabilities) changed —
    /// those only travel inside full inventory reports.
    pub fn spawn_health_loop(
        &self,
        sched: crate::scheduler::LocalScheduler,
        capacity_notify: std::sync::Arc<tokio::sync::Notify>,
        inventory_notify: std::sync::Arc<tokio::sync::Notify>,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = this.inner.cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let (ready_changed, inv_changed) = this.health_pass(&sched).await;
                        if ready_changed {
                            capacity_notify.notify_one();
                        }
                        if inv_changed {
                            inventory_notify.notify_one();
                        }
                    }
                }
            }
        });
    }

    /// Returns (readiness_changed, inventory_changed): whether any
    /// deployment's readiness flipped this pass, and whether any
    /// capability table was newly filled (probe/override) — the latter only
    /// reaches the server inside a full inventory report.
    async fn health_pass(&self, sched: &crate::scheduler::LocalScheduler) -> (bool, bool) {
        let mut changed = false;
        let mut inv_changed = false;
        // Probe each backend; mark deployments accordingly.
        let backend_ids: Vec<String> = self.inner.backends.read().await.keys().cloned().collect();
        for bid in backend_ids {
            let (adapter_health, installed, loaded, bindable, remote) = {
                let backends = self.inner.backends.read().await;
                let Some(be) = backends.get(&bid) else {
                    continue;
                };
                if !be.cfg.enabled {
                    continue;
                }
                let h = be.adapter.health().await;
                *be.health.write().await = h;
                // Err = installed-set unknown → permissive rather than a
                // wrong "not installed" conclusion on a flaky list call.
                let installed = be.adapter.list_models().await.ok();
                if let Some(m) = &installed {
                    *be.models.write().await = m.clone();
                }
                let loaded = if be.cfg.remote {
                    None
                } else {
                    be.adapter.loaded_models().await
                };
                *be.loaded.write().await = loaded.clone();
                let bindable = if be.cfg.remote {
                    None
                } else {
                    be.adapter.bindable_models().await
                };
                *be.bindable.write().await = bindable.clone();
                (h, installed, loaded, bindable, be.cfg.remote)
            };

            let dep_ids: Vec<String> = {
                let deps = self.inner.deployments.read().await;
                deps.iter()
                    .filter(|(_, d)| d.backend_id == bid)
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            for did in dep_ids {
                let deps = self.inner.deployments.read().await;
                let Some(de) = deps.get(&did) else { continue };
                *de.health.write().await = adapter_health;

                // Tier-2 readiness: backend up AND model servable now.
                //  - remote backends: always ready (hosted, no local model)
                //  - installed-set known & model missing → not installed
                //  - auto_load deployments must be resident (loaded-set
                //    known & model absent = evicted → gate until re-warm)
                //  - auto_load=false: JIT loads on first request → stay ready
                //  - model_name="auto" (local only): follows the backend's
                //    current model — ready while one resolves
                let health_ok = adapter_health == Health::Healthy;
                let dynamic = de.cfg.model_name == loom_core::config::DYNAMIC_MODEL && !remote;
                let effective_model = if dynamic {
                    let resolved =
                        Self::resolve_dynamic(&de.warned_multi, &did, &bindable, &installed);
                    let mut rm = de.resolved_model.write().await;
                    if *rm != resolved {
                        if let Some(old) = rm.as_ref() {
                            info!(deployment = %did, from = %old, to = ?resolved,
                                  "dynamic deployment re-bound to a different model");
                        }
                        *rm = resolved.clone();
                        // Model changed → previously probed caps describe the
                        // old model; re-probe before the next report.
                        *de.caps.write().await = None;
                        inv_changed = true;
                        changed = true;
                    }
                    resolved
                } else {
                    None
                };
                let (ready, model, loaded_ok) = if dynamic {
                    (
                        effective_model.is_some(),
                        effective_model.clone().unwrap_or_default(),
                        effective_model.is_some(),
                    )
                } else {
                    let model = de.cfg.model_name.as_str();
                    let installed_ok = remote
                        || installed
                            .as_ref()
                            .map(|l| l.iter().any(|m| m == model))
                            .unwrap_or(true);
                    let loaded_ok = remote
                        || !de.cfg.auto_load
                        || loaded
                            .as_ref()
                            .map(|l| l.iter().any(|m| m == model))
                            .unwrap_or(true);
                    (installed_ok && loaded_ok, model.to_string(), loaded_ok)
                };
                changed |= sched.set_dep_ready(&did, health_ok, ready);
                // Auto-load: warm the model once when the backend is healthy
                // and the deployment is enabled+idle (re-warms after recovery).
                // Keep-alive warm: JIT backends (LM Studio) evict idle models,
                // so re-warm every ~4 min while healthy rather than once.
                // Not-loaded retries run on a 60s cadence — fast enough to
                // re-warm real evictions, but not a 10s hammer against a
                // model the backend refuses to load.
                // Eviction ping-pong: a warm that "succeeded" but left the
                // model absent (evicted by a co-resident model's warm/load)
                // doubles the retry cadence up to 30min — otherwise two
                // auto_load models fight over limited memory forever.
                let warmed_before = de.warmed_at.read().await.is_some();
                if de.cfg.auto_load && warmed_before && !loaded_ok {
                    // Count once per absent streak — a model that stays evicted
                    // must not escalate the backoff per probe tick.
                    if !de
                        .miss_counted
                        .swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
                        let misses = de
                            .evict_misses
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        if misses == 2 {
                            warn!(deployment = %model, "model keeps getting evicted after warm — backing off auto-load");
                        }
                    }
                } else if loaded_ok {
                    de.evict_misses
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    de.miss_counted
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                let base = if loaded_ok { 240u64 } else { 60 };
                let rewarm_secs = base
                    .saturating_mul(
                        1u64 << de
                            .evict_misses
                            .load(std::sync::atomic::Ordering::Relaxed)
                            .min(5),
                    )
                    .min(1800);
                let mut w = de.warmed_at.write().await;
                if adapter_health != Health::Healthy {
                    *w = None;
                } else if de.cfg.auto_load
                    && de.cfg.enabled
                    // Dynamic deployments follow the resident model — there
                    // is nothing to warm.
                    && !dynamic
                    // While the tier-2 gate is closed (local pressure), a
                    // pressure-offloaded model must stay unloaded — the local
                    // path still loads it JIT on demand.
                    && !sched.tier2_gated(&did)
                    // VRAM full → don't even attempt a load that cannot fit.
                    && !self.vram_blocked().await
                    && w.map(|t| t.elapsed().as_secs() >= rewarm_secs)
                        .unwrap_or(true)
                {
                    *w = Some(std::time::Instant::now());
                    let inner = self.inner.clone();
                    let (bid2, model) = (bid.clone(), de.cfg.model_name.clone());
                    tokio::spawn(async move {
                        let backends = inner.backends.read().await;
                        if let Some(be) = backends.get(&bid2) {
                            // Hosted providers hold no local model — warm-up is
                            // meaningless and would only burn a paid request.
                            if be.cfg.remote {
                                return;
                            }
                            match be.adapter.warm_up(&model).await {
                                Ok(_) => info!(deployment = %model, "model warmed"),
                                Err(e) => {
                                    warn!(deployment = %model, error = %e, "auto-load failed")
                                }
                            }
                        }
                    });
                }
                drop(w);
                // capability_overrides: skip probing entirely — the config's
                // own capability table IS the authoritative report.
                if de.cfg.capability_overrides && de.caps.read().await.is_none() {
                    *de.caps.write().await = Some(de.cfg.capabilities.clone());
                    inv_changed = true;
                }
                // Probe capabilities once (or when empty) unless overridden.
                // Dynamic deps probe the resolved model — nothing to probe
                // while no model is bound.
                let needs_probe = !de.cfg.capability_overrides
                    && de.caps.read().await.is_none()
                    && adapter_health != Health::Down
                    && !model.is_empty();
                if needs_probe {
                    let backends = self.inner.backends.read().await;
                    if let Some(be) = backends.get(&bid) {
                        match be.adapter.probe_capabilities(&model).await {
                            Ok(mut probed) => {
                                // Manual capability fields from config merge in.
                                merge_overrides(&mut probed, &de.cfg.capabilities);
                                *de.caps.write().await = Some(probed);
                                inv_changed = true;
                            }
                            Err(e) => {
                                warn!(deployment = %did, error = %e, "capability probe failed");
                            }
                        }
                    }
                }
            }
        }
        (changed, inv_changed)
    }

    /// Resolve a `model_name == "auto"` deployment: the last entry of the
    /// backend's loaded set wins (registry order — vLLM's single loaded
    /// model is the common case), falling back to the first listed model
    /// when the backend exposes no loaded-set. Multiple loaded models log
    /// a once-per-deployment warning.
    fn resolve_dynamic(
        warned_multi: &std::sync::atomic::AtomicBool,
        dep_id: &str,
        loaded: &Option<Vec<String>>,
        installed: &Option<Vec<String>>,
    ) -> Option<String> {
        match loaded.as_deref() {
            Some([only]) => Some(only.clone()),
            Some([]) => installed.as_ref().and_then(|l| l.first().cloned()),
            Some(many) => {
                if !warned_multi.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    warn!(deployment = %dep_id, models = ?many,
                          "multiple bindable models loaded — dynamic deployment binds the last entry");
                }
                many.last().cloned()
            }
            // No loaded-set (plain OpenAI servers): the listed set IS what's
            // served — first entry.
            None => installed.as_ref().and_then(|l| l.first().cloned()),
        }
    }

    /// Effective (resolved) model of a deployment — static name or, for
    /// `model_name == "auto"` deployments, the bound model.
    pub async fn effective_model(&self, dep_id: &str) -> Option<String> {
        let deps = self.inner.deployments.read().await;
        let de = deps.get(dep_id)?;
        if de.cfg.model_name != loom_core::config::DYNAMIC_MODEL {
            return Some(de.cfg.model_name.clone());
        }
        let resolved = de.resolved_model.read().await.clone();
        resolved
    }

    /// Detected-but-undeployed models per backend: listed set minus models
    /// covered by a deployment (static name or resolved dynamic binding),
    /// with loaded-state per model. Local backends only.
    pub async fn detected_models(&self) -> HashMap<String, Vec<(String, bool)>> {
        let deps = self.inner.deployments.read().await;
        let mut covered: HashMap<String, Vec<String>> = HashMap::new();
        for d in deps.values() {
            let m = if d.cfg.model_name == loom_core::config::DYNAMIC_MODEL {
                d.resolved_model.read().await.clone()
            } else {
                Some(d.cfg.model_name.clone())
            };
            if let Some(m) = m {
                covered.entry(d.backend_id.clone()).or_default().push(m);
            }
        }
        drop(deps);
        let backends = self.inner.backends.read().await;
        let mut out = HashMap::new();
        for (bid, be) in backends.iter() {
            if be.cfg.remote {
                continue;
            }
            let installed = be.models.read().await.clone();
            let loaded: Vec<String> = be.loaded.read().await.clone().unwrap_or_default();
            let cov = covered.get(bid).cloned().unwrap_or_default();
            let detected: Vec<(String, bool)> = installed
                .into_iter()
                .map(|m| (m.clone(), loaded.contains(&m), cov.contains(&m)))
                .filter(|(_, _, c)| !*c)
                .map(|(m, l, _)| (m, l))
                .collect();
            out.insert(bid.clone(), detected);
        }
        out
    }

    /// Force re-probe all deployments of a backend.
    pub async fn reprobe(&self) {
        let dep_ids: Vec<String> = self
            .inner
            .deployments
            .read()
            .await
            .keys()
            .cloned()
            .collect();
        for did in dep_ids {
            let (backend_id, model) = {
                let deps = self.inner.deployments.read().await;
                let Some(d) = deps.get(&did) else { continue };
                (d.backend_id.clone(), d.cfg.model_name.clone())
            };
            let backends = self.inner.backends.read().await;
            if let Some(be) = backends.get(&backend_id) {
                if let Ok(mut probed) = be.adapter.probe_capabilities(&model).await {
                    let deps = self.inner.deployments.read().await;
                    if let Some(de) = deps.get(&did) {
                        if de.cfg.capability_overrides {
                            merge_overrides(&mut probed, &de.cfg.capabilities);
                        }
                        *de.caps.write().await = Some(probed);
                        info!(deployment = %did, "re-probed capabilities");
                    }
                }
            }
        }
    }

    /// Deployment lookup.
    pub async fn deployment(&self, id: &str) -> Option<DeploymentInfo> {
        let (cfg, backend_id, caps, health) = {
            let deps = self.inner.deployments.read().await;
            let d = deps.get(id)?;
            (
                d.cfg.clone(),
                d.backend_id.clone(),
                d.caps.clone(),
                d.health.clone(),
            )
        };
        let capabilities = caps.read().await.clone();
        let health = *health.read().await;
        Some(DeploymentInfo {
            cfg,
            backend_id,
            capabilities,
            health,
        })
    }

    /// Resolve `model` (deployment name, deployment id, or physical model name)
    /// to a deployment id. Prefer enabled deployments.
    pub async fn resolve_model(&self, model: &str) -> Option<String> {
        let deps = self.inner.deployments.read().await;
        // 1) exact deployment id
        if deps.contains_key(model) {
            return Some(model.to_string());
        }
        // 2) deployment name
        for (id, d) in deps.iter() {
            if d.cfg.name == model && d.cfg.enabled {
                return Some(id.clone());
            }
        }
        // 3) physical model name (first enabled match)
        for (id, d) in deps.iter() {
            if d.cfg.model_name == model && d.cfg.enabled {
                return Some(id.clone());
            }
        }
        // 4) dynamically-bound model of an "auto" deployment — explicit
        // static deployments already won above.
        for (id, d) in deps.iter() {
            if !d.cfg.enabled || d.cfg.model_name != loom_core::config::DYNAMIC_MODEL {
                continue;
            }
            if d.resolved_model.read().await.as_deref() == Some(model) {
                return Some(id.clone());
            }
        }
        None
    }

    /// Forward a request to the backend owning the deployment.
    pub async fn forward(
        &self,
        dep_id: &str,
        api: ApiType,
        body: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<crate::adapters::BackendResponse, AdapterError> {
        let (backend_id, model) = self.deployment_model(dep_id).await?;
        let backends = self.inner.backends.read().await;
        let Some(be) = backends.get(&backend_id) else {
            return Err(AdapterError::Connect(format!(
                "missing backend {backend_id}"
            )));
        };
        if !be.cfg.enabled {
            return Err(AdapterError::Connect("backend disabled".into()));
        }
        be.adapter.forward(api, &model, body, cancel).await
    }

    /// Model a request should carry upstream: static `model_name`, or — for
    /// `"auto"` deployments — the stored resolution, recomputed live from the
    /// backend's model sets when health_pass has not populated it yet.
    async fn deployment_model(&self, dep_id: &str) -> Result<(String, String), AdapterError> {
        let (backend_id, model_name, resolved, warned) = {
            let deps = self.inner.deployments.read().await;
            let Some(d) = deps.get(dep_id) else {
                return Err(AdapterError::Protocol(format!(
                    "unknown deployment {dep_id}"
                )));
            };
            let resolved = d.resolved_model.read().await.clone();
            (
                d.backend_id.clone(),
                d.cfg.model_name.clone(),
                resolved,
                d.warned_multi.clone(),
            )
        };
        let backends = self.inner.backends.read().await;
        let Some(be) = backends.get(&backend_id) else {
            return Err(AdapterError::Connect(format!(
                "missing backend {backend_id}"
            )));
        };
        // "auto" on a remote/hosted backend has no resident-model concept —
        // a manually-edited config falls through and upstreams the literal
        // name, which fails cleanly there.
        if model_name != loom_core::config::DYNAMIC_MODEL || be.cfg.remote {
            return Ok((backend_id, model_name));
        }
        if let Some(m) = resolved {
            return Ok((backend_id, m));
        }
        let loaded = be.bindable.read().await.clone();
        let installed = Some(be.models.read().await.clone());
        Self::resolve_dynamic(&warned, dep_id, &loaded, &installed)
            .map(|m| (backend_id.clone(), m))
            .ok_or_else(|| AdapterError::Connect("auto deployment: no model loaded".into()))
    }

    /// Unload a deployment's model from backend memory (pressure offload).
    pub async fn unload(&self, dep_id: &str) -> Result<(), AdapterError> {
        let (backend_id, model) = self.deployment_model(dep_id).await?;
        let backends = self.inner.backends.read().await;
        let Some(be) = backends.get(&backend_id) else {
            return Err(AdapterError::Connect(format!(
                "missing backend {backend_id}"
            )));
        };
        be.adapter.unload(&model).await
    }

    /// Raw multipart forward (audio transcription/translation). The adapter
    /// rewrites the `model` form field to the physical model name.
    pub async fn forward_multipart(
        &self,
        dep_id: &str,
        api: ApiType,
        body: &[u8],
        content_type: &str,
        cancel: CancellationToken,
    ) -> Result<crate::adapters::BackendResponse, AdapterError> {
        let (backend_id, model) = self.deployment_model(dep_id).await?;
        let backends = self.inner.backends.read().await;
        let Some(be) = backends.get(&backend_id) else {
            return Err(AdapterError::Connect(format!(
                "missing backend {backend_id}"
            )));
        };
        if !be.cfg.enabled {
            return Err(AdapterError::Connect("backend disabled".into()));
        }
        be.adapter
            .forward_multipart(api, &model, body, content_type, cancel)
            .await
    }

    /// Full inventory for server reports.
    pub async fn inventory(&self) -> Vec<loom_protocol::pb::DeploymentReport> {
        let deps = self.inner.deployments.read().await;
        let backends = self.inner.backends.read().await;
        let mut out = Vec::new();
        for (id, d) in deps.iter() {
            let bt = backends
                .get(&d.backend_id)
                .map(|b| b.cfg.backend_type)
                .unwrap_or(BackendType::GenericOpenAi);
            let caps = d.caps.read().await.clone();
            let health = *d.health.read().await;
            let model_name = if d.cfg.model_name == loom_core::config::DYNAMIC_MODEL {
                d.resolved_model
                    .read()
                    .await
                    .clone()
                    .unwrap_or_else(|| d.cfg.model_name.clone())
            } else {
                d.cfg.model_name.clone()
            };
            out.push(loom_protocol::pb::DeploymentReport {
                deployment_id: id.clone(),
                name: d.cfg.name.clone(),
                backend_type: bt.as_str().into(),
                endpoint_url: backends
                    .get(&d.backend_id)
                    .map(|b| b.cfg.endpoint.clone())
                    .unwrap_or_default(),
                model_name,
                capabilities: caps.as_ref().map(caps_to_pb),
                enabled: d.cfg.enabled,
                health: health.as_str().into(),
            });
        }
        out
    }

    /// List exposed model names for the local OpenAI API (deployment names).
    pub async fn local_models(&self) -> Vec<String> {
        let deps = self.inner.deployments.read().await;
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for d in deps.values().filter(|d| d.cfg.enabled) {
            let m = if d.cfg.model_name == loom_core::config::DYNAMIC_MODEL {
                match d.resolved_model.read().await.clone() {
                    Some(m) => m,
                    None => continue,
                }
            } else {
                d.cfg.model_name.clone()
            };
            if seen.insert(m.clone()) {
                out.push(m);
            }
        }
        out
    }

    /// Capability of a deployment for local API checks.
    pub async fn capabilities(&self, dep_id: &str) -> Option<Capabilities> {
        let caps = {
            let deps = self.inner.deployments.read().await;
            deps.get(dep_id)?.caps.clone()
        };
        // None covers both "missing" and "not yet probed" (permissive).
        let v = caps.read().await.clone();
        v
    }

    /// Health of a deployment.
    pub async fn health_of(&self, dep_id: &str) -> Option<Health> {
        let health = {
            let deps = self.inner.deployments.read().await;
            deps.get(dep_id)?.health.clone()
        };
        let v = *health.read().await;
        Some(v)
    }

    /// Backend type for a deployment (for reports/UI).
    pub async fn backend_type_of(&self, dep_id: &str) -> Option<BackendType> {
        let backend_id = {
            let deps = self.inner.deployments.read().await;
            deps.get(dep_id)?.backend_id.clone()
        };
        let backends = self.inner.backends.read().await;
        backends.get(&backend_id).map(|b| b.cfg.backend_type)
    }

    pub fn shutdown(&self) {
        self.inner.cancel.cancel();
    }
}

fn merge_overrides(probed: &mut Capabilities, overrides: &Capabilities) {
    // Overrides are only meaningful where they differ from defaults; simplest
    // robust rule: OR-enable boolean caps, take context_length if set, union
    // modalities.
    probed.chat |= overrides.chat;
    probed.completions |= overrides.completions;
    probed.embeddings |= overrides.embeddings;
    probed.images |= overrides.images;
    probed.audio_speech |= overrides.audio_speech;
    probed.audio_transcription |= overrides.audio_transcription;
    probed.audio_translation |= overrides.audio_translation;
    probed.video |= overrides.video;
    probed.tools |= overrides.tools;
    probed.vision |= overrides.vision;
    probed.json_mode |= overrides.json_mode;
    probed.streaming |= overrides.streaming;
    if overrides.context_length > 0 {
        probed.context_length = overrides.context_length;
    }
    for m in &overrides.modalities {
        probed.modalities.insert(m.clone());
    }
}

pub fn caps_to_pb(c: &Capabilities) -> loom_protocol::pb::Capabilities {
    loom_protocol::pb::Capabilities {
        chat: c.chat,
        completions: c.completions,
        embeddings: c.embeddings,
        tools: c.tools,
        vision: c.vision,
        json_mode: c.json_mode,
        streaming: c.streaming,
        context_length: c.context_length,
        modalities: c.modalities.iter().cloned().collect(),
        images: c.images,
        audio_speech: c.audio_speech,
        audio_transcription: c.audio_transcription,
        audio_translation: c.audio_translation,
        video: c.video,
    }
}

pub fn caps_from_pb(c: &loom_protocol::pb::Capabilities) -> Capabilities {
    Capabilities {
        chat: c.chat,
        completions: c.completions,
        embeddings: c.embeddings,
        tools: c.tools,
        vision: c.vision,
        json_mode: c.json_mode,
        streaming: c.streaming,
        context_length: c.context_length,
        modalities: c.modalities.iter().cloned().collect(),
        images: c.images,
        audio_speech: c.audio_speech,
        audio_transcription: c.audio_transcription,
        audio_translation: c.audio_translation,
        video: c.video,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn dyn_resolve(loaded: Option<Vec<&str>>, installed: Option<Vec<&str>>) -> Option<String> {
        let warned = AtomicBool::new(false);
        let l = loaded.map(|v| v.into_iter().map(String::from).collect());
        let i = installed.map(|v| v.into_iter().map(String::from).collect());
        BackendManager::resolve_dynamic(&warned, "dep", &l, &i)
    }

    #[test]
    fn resolve_dynamic_single_loaded() {
        assert_eq!(
            dyn_resolve(Some(vec!["m1"]), Some(vec!["m1", "m2"])).as_deref(),
            Some("m1")
        );
    }

    #[test]
    fn resolve_dynamic_multi_loaded_binds_last() {
        // vLLM-style: last loaded entry wins; order is the backend's
        // registry order, deterministic across probes.
        assert_eq!(
            dyn_resolve(Some(vec!["m1", "m2"]), Some(vec!["m1", "m2", "m3"])).as_deref(),
            Some("m2")
        );
    }

    #[test]
    fn resolve_dynamic_empty_loaded_falls_back_to_first_installed() {
        assert_eq!(
            dyn_resolve(Some(vec![]), Some(vec!["m1", "m2"])).as_deref(),
            Some("m1")
        );
    }

    #[test]
    fn resolve_dynamic_no_loaded_set_uses_listed() {
        // Plain OpenAI servers: loaded-set unknown → listed set is served.
        assert_eq!(
            dyn_resolve(None, Some(vec!["m1", "m2"])).as_deref(),
            Some("m1")
        );
    }

    #[test]
    fn resolve_dynamic_nothing_available() {
        assert_eq!(dyn_resolve(Some(vec![]), None), None);
        assert_eq!(dyn_resolve(None, None), None);
    }
}
