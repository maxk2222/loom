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
    /// GPU VRAM usage limit consulted by auto-load warm-up (0 = ignore).
    pub gpu_vram_limit_pct: u8,
    pub enabled: bool,
    /// GPU sample — `None` when no GPU was detected or probing failed.
    /// On Apple Silicon `utilization`/`memory` stay empty: Metal shares the
    /// unified RAM already reported above, and utilization needs sudo.
    pub gpu: Option<GpuStats>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuStats {
    pub name: String,
    /// GPU core utilization 0-100% (None = unsupported on this platform).
    pub util_pct: Option<f32>,
    /// Dedicated VRAM (None on unified-memory platforms).
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
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
    spawn_gpu_sampler(stats.clone());
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
                w.cpu_pct = cpu;
                w.ram_pct = ram_pct;
                w.ram_used_bytes = ram_used;
                w.ram_total_bytes = ram_total;
                w.pressure = over;
                w.cooldown = !over && gated;
                w.cpu_limit_pct = cfg.cpu_pct;
                w.ram_limit_pct = cfg.ram_pct;
                w.gpu_vram_limit_pct = cfg.gpu_vram_pct;
                w.enabled = limits_on;
            }
        }
    });
    stats
}

/// GPU sampler — independent cadence (~15s) because probing shells out.
/// Linux+NVIDIA: `nvidia-smi` (the dominant inference case). macOS: GPU name
/// only via `system_profiler` — Metal shares unified RAM and utilization
/// requires root. Other platforms leave `gpu` unset.
fn spawn_gpu_sampler(stats: SharedStats) {
    tokio::spawn(async move {
        if cfg!(target_os = "macos") {
            if let Some(g) = probe_macos_gpu().await {
                if let Ok(mut w) = stats.write() {
                    w.gpu = Some(g);
                }
            }
            return;
        }
        if !has_nvidia_smi().await {
            return;
        }
        loop {
            if let Some(g) = probe_nvidia().await {
                if let Ok(mut w) = stats.write() {
                    w.gpu = Some(g);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    });
}

async fn probe_nvidia() -> Option<GpuStats> {
    let out = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Aggregate across GPUs: max utilization, summed memory.
    let mut name = String::new();
    let mut util = 0.0f32;
    let (mut used, mut total) = (0u64, 0u64);
    for line in text.lines() {
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if cols.len() < 4 {
            continue;
        }
        if name.is_empty() {
            name = cols[0].to_string();
        }
        util = util.max(cols[1].parse().unwrap_or(0.0));
        used += cols[2].parse::<u64>().unwrap_or(0) * 1_048_576; // MiB → B
        total += cols[3].parse::<u64>().unwrap_or(0) * 1_048_576;
    }
    (!name.is_empty()).then_some(GpuStats {
        name,
        util_pct: Some(util),
        mem_used_bytes: Some(used),
        mem_total_bytes: Some(total),
    })
}

async fn has_nvidia_smi() -> bool {
    tokio::process::Command::new("nvidia-smi")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
async fn probe_macos_gpu() -> Option<GpuStats> {
    let out = tokio::process::Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // First "Chipset Model:" line is the primary GPU.
    let name = text
        .lines()
        .find_map(|l| {
            l.split_once("Chipset Model:")
                .map(|(_, v)| v.trim().to_string())
        })
        .filter(|s| !s.is_empty())?;
    Some(GpuStats {
        name,
        util_pct: None,
        mem_used_bytes: None,
        mem_total_bytes: None,
    })
}

#[cfg(not(target_os = "macos"))]
async fn probe_macos_gpu() -> Option<GpuStats> {
    None
}
