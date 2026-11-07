//! System pressure monitor: samples host CPU/RAM and closes every shared
//! deployment's tier-2 gate when local load exceeds configured limits.
//!
//! Local-first guarantee: under pressure the client stops *accepting* new
//! tier-2 work (accepting=false → the server routes elsewhere). In-flight
//! requests are never preempted. With `unload_models` enabled, fully idle
//! deployments also unload their model to actually free memory; auto_load
//! deployments re-warm automatically once pressure clears.
//!
//! Reopening uses a cooldown: pressure must stay below limits for
//! `cooldown_secs` so the gate does not flap on noisy load spikes.

use crate::backend_manager::BackendManager;
use crate::scheduler::LocalScheduler;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tracing::{info, warn};

/// Latest sampled system state, shared with the local API for the dashboard.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SystemStats {
    /// Whole-system CPU usage 0-100%.
    pub cpu_pct: f32,
    /// Whole-system RAM usage 0-100%.
    pub ram_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    /// True while a configured limit is exceeded (gate closed).
    pub pressure: bool,
    /// True while gates stay closed during the post-pressure cooldown.
    pub cooldown: bool,
    /// Configured limits in effect (0 = that metric ignored).
    pub cpu_limit_pct: u8,
    pub ram_limit_pct: u8,
    pub enabled: bool,
}

pub type SharedStats = Arc<RwLock<SystemStats>>;

const SAMPLE_SECS: u64 = 3;

/// Spawn the sampling loop. Re-reads `PressureConfig` each tick so UI config
/// edits take effect without a restart. Returns the shared stats handle.
pub fn spawn(
    cfg_store: Arc<crate::config::ConfigStore>,
    sched: LocalScheduler,
    backends: BackendManager,
    capacity_notify: Arc<tokio::sync::Notify>,
) -> SharedStats {
    let stats: SharedStats = Arc::new(RwLock::new(SystemStats::default()));
    let stats2 = stats.clone();
    tokio::spawn(async move {
        let mut sys = sysinfo::System::new();
        let mut below_since: Option<Instant> = None;
        let mut gated = false;
        // Give sysinfo a tick to produce a meaningful cpu delta.
        sys.refresh_cpu_usage();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SAMPLE_SECS)).await;
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            let cfg = cfg_store.get().await.pressure;

            let cpu = sys.global_cpu_usage();
            let ram_used = sys.used_memory();
            let ram_total = sys.total_memory().max(1);
            let ram_pct = ram_used as f32 * 100.0 / ram_total as f32;

            let limits_on = cfg.enabled && (cfg.cpu_pct > 0 || cfg.ram_pct > 0);
            let over = limits_on
                && ((cfg.cpu_pct > 0 && cpu >= cfg.cpu_pct as f32)
                    || (cfg.ram_pct > 0 && ram_pct >= cfg.ram_pct as f32));

            // Cooldown bookkeeping.
            let now = Instant::now();
            if over {
                below_since = None;
            } else {
                below_since.get_or_insert(now);
            }
            // Gate state: closed while over, reopens after cooldown below.
            let want_gate = if over {
                true
            } else if gated {
                below_since
                    .map(|t| t.elapsed().as_secs() < cfg.cooldown_secs)
                    .unwrap_or(true)
            } else {
                false
            };

            if want_gate != gated {
                gated = want_gate;
                if sched.apply_pressure_gate(gated) {
                    capacity_notify.notify_one();
                }
                if gated {
                    warn!(cpu = %cpu, ram = %ram_pct, "local pressure — tier-2 gates closed");
                } else {
                    info!(cpu = %cpu, ram = %ram_pct, "pressure cleared — tier-2 gates open");
                }
            }

            // Offload: while gated, unload models on shared deployments with
            // zero in-flight work. Private (local-only) deployments are the
            // user's own and are never touched. Failures are per-deployment
            // and non-fatal — the gate alone already protects local resources.
            if gated && cfg.unload_models {
                let shared: std::collections::HashSet<String> = sched
                    .capacity_snapshot()
                    .into_iter()
                    .filter(|c| c.sharing_mode != loom_core::config::SharingMode::Private)
                    .map(|c| c.deployment_id)
                    .collect();
                for dep_id in sched.idle_deployments() {
                    if !shared.contains(&dep_id) {
                        continue;
                    }
                    match backends.unload(&dep_id).await {
                        Ok(()) => info!(deployment = %dep_id, "model unloaded (pressure offload)"),
                        Err(e) => {
                            tracing::debug!(deployment = %dep_id, error = %e, "pressure unload skipped")
                        }
                    }
                }
            }

            if let Ok(mut w) = stats2.write() {
                *w = SystemStats {
                    cpu_pct: cpu,
                    ram_pct,
                    ram_used_bytes: ram_used,
                    ram_total_bytes: ram_total,
                    pressure: over,
                    cooldown: !over && gated,
                    cpu_limit_pct: cfg.cpu_pct,
                    ram_limit_pct: cfg.ram_pct,
                    enabled: limits_on,
                };
            }
        }
    });
    stats
}
