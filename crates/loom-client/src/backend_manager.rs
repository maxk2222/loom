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
    cancel: CancellationToken,
}

struct BackendEntry {
    cfg: BackendConfig,
    adapter: Box<dyn BackendAdapter>,
    health: Arc<RwLock<Health>>,
    models: Arc<RwLock<Vec<String>>>,
}

struct DeploymentEntry {
    cfg: DeploymentConfig,
    backend_id: String,
    caps: Arc<RwLock<Option<Capabilities>>>,
    health: Arc<RwLock<Health>>,
    warmed_at: Arc<RwLock<Option<std::time::Instant>>>,
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
                cancel: CancellationToken::new(),
            }),
        }
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
                },
            );

            for d in &b.deployments {
                let caps = deployments
                    .remove(&d.id)
                    .map(|e| e.caps)
                    .unwrap_or_else(|| Arc::new(RwLock::new(None)));
                next_deployments.insert(
                    d.id.clone(),
                    DeploymentEntry {
                        warmed_at: Arc::new(RwLock::new(None)),
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
    pub fn spawn_health_loop(
        &self,
        sched: crate::scheduler::LocalScheduler,
        capacity_notify: std::sync::Arc<tokio::sync::Notify>,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = this.inner.cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if this.health_pass(&sched).await {
                            capacity_notify.notify_one();
                        }
                    }
                }
            }
        });
    }

    /// Returns true when any deployment's readiness changed this pass.
    async fn health_pass(&self, sched: &crate::scheduler::LocalScheduler) -> bool {
        let mut changed = false;
        // Probe each backend; mark deployments accordingly.
        let backend_ids: Vec<String> = self.inner.backends.read().await.keys().cloned().collect();
        for bid in backend_ids {
            let (adapter_health, installed, loaded, remote) = {
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
                (h, installed, loaded, be.cfg.remote)
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
                let health_ok = adapter_health == Health::Healthy;
                let model = &de.cfg.model_name;
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
                let ready = installed_ok && loaded_ok;
                changed |= sched.set_dep_ready(&did, health_ok, ready);
                // Auto-load: warm the model once when the backend is healthy
                // and the deployment is enabled+idle (re-warms after recovery).
                // Keep-alive warm: JIT backends (LM Studio) evict idle models,
                // so re-warm every ~4 min while healthy rather than once.
                let mut w = de.warmed_at.write().await;
                if adapter_health != Health::Healthy {
                    *w = None;
                } else if de.cfg.auto_load
                    && de.cfg.enabled
                    // Re-warm on the 4-min keep-alive cadence OR immediately
                    // when the model was just evicted (ready flipped false).
                    && (!loaded_ok
                        || w.map(|t| t.elapsed().as_secs() >= 240).unwrap_or(true))
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
                // Probe capabilities once (or when empty) unless overridden.
                let needs_probe = !de.cfg.capability_overrides
                    && de.caps.read().await.is_none()
                    && adapter_health != Health::Down;
                if needs_probe {
                    let backends = self.inner.backends.read().await;
                    if let Some(be) = backends.get(&bid) {
                        match be.adapter.probe_capabilities(&de.cfg.model_name).await {
                            Ok(mut probed) => {
                                // Manual capability fields from config merge in.
                                merge_overrides(&mut probed, &de.cfg.capabilities);
                                *de.caps.write().await = Some(probed);
                            }
                            Err(e) => {
                                warn!(deployment = %did, error = %e, "capability probe failed");
                            }
                        }
                    }
                }
            }
        }
        changed
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
        let (backend_id, model) = {
            let deps = self.inner.deployments.read().await;
            let Some(d) = deps.get(dep_id) else {
                return Err(AdapterError::Protocol(format!(
                    "unknown deployment {dep_id}"
                )));
            };
            (d.backend_id.clone(), d.cfg.model_name.clone())
        };
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
            out.push(loom_protocol::pb::DeploymentReport {
                deployment_id: id.clone(),
                name: d.cfg.name.clone(),
                backend_type: bt.as_str().into(),
                endpoint_url: backends
                    .get(&d.backend_id)
                    .map(|b| b.cfg.endpoint.clone())
                    .unwrap_or_default(),
                model_name: d.cfg.model_name.clone(),
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
        deps.values()
            .filter(|d| d.cfg.enabled)
            .filter(|d| seen.insert(d.cfg.model_name.clone()))
            .map(|d| d.cfg.model_name.clone())
            .collect()
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
    }
}
