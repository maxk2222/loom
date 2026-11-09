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
        // Deployments whose adapter reported unload unsupported — skip them
        // for the rest of this pressure event instead of retrying every
        // tick. Reset when gates reopen.
        let mut offload_unsupported: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // refresh_cpu_all() both discovers the cpu structs and refreshes
        // usage — refresh_cpu_usage() alone updates only existing entries,
        // which leaves global_cpu_usage() at 0 on Linux where the list
        // starts empty. Give sysinfo a tick to produce a meaningful delta.
        sys.refresh_cpu_all();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SAMPLE_SECS)).await;
            sys.refresh_cpu_all();
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
                    offload_unsupported.clear();
                    info!(cpu = %cpu, ram = %ram_pct, "pressure cleared — tier-2 gates open");
                }
            }

            // Offload: while gated, unload models on shared deployments with
            // zero in-flight work. Private (local-only) deployments are the
            // user's own and are never touched. Failures are per-deployment
            // and non-fatal — the gate alone already protects local resources.
            if gated && cfg.unload_models {
                let snapshot = sched.capacity_snapshot();
                // Only shared deployments whose model is actually resident —
                // unloading an absent model is a no-op that would spam the
                // log every tick for the whole pressure event.
                let unloadable: std::collections::HashSet<String> = snapshot
                    .iter()
                    .filter(|c| {
                        c.sharing_mode != loom_core::config::SharingMode::Private && c.model_ready
                    })
                    .map(|c| c.deployment_id.clone())
                    .collect();
                for dep_id in sched.idle_deployments() {
                    if !unloadable.contains(&dep_id) || offload_unsupported.contains(&dep_id) {
                        continue;
                    }
                    match backends.unload(&dep_id).await {
                        Ok(()) => {
                            // The model is no longer resident — reflect that
                            // immediately instead of waiting ~15s for the next
                            // health probe; also keeps the next tick from
                            // re-unloading an already-absent model.
                            let healthy = snapshot
                                .iter()
                                .find(|c| c.deployment_id == dep_id)
                                .map(|c| c.health_ok)
                                .unwrap_or(true);
                            let _ = sched.set_dep_ready(&dep_id, healthy, false);
                            capacity_notify.notify_one();
                            info!(deployment = %dep_id, "model unloaded (pressure offload)");
                        }
                        Err(e) => {
                            if matches!(e, crate::adapters::AdapterError::Protocol(_)) {
                                offload_unsupported.insert(dep_id.clone());
                            }
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
/// Linux+NVIDIA: `nvidia-smi` (the dominant inference case). macOS: name via
/// `system_profiler` once, then IOGPU PerformanceStatistics via `ioreg` —
/// Device Utilization % + in-use/allocated system memory (no root needed on
/// Apple Silicon). Other platforms leave `gpu` unset.
fn spawn_gpu_sampler(stats: SharedStats) {
    tokio::spawn(async move {
        if cfg!(target_os = "macos") {
            loop {
                if let Some(g) = probe_macos_gpu().await {
                    if let Ok(mut w) = stats.write() {
                        w.gpu = Some(g);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
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
    // Aggregate across GPUs: max utilization, summed memory. Fields the
    // driver reports as "[N/A]" (e.g. FB memory on unified-memory GB10)
    // stay None instead of collapsing to a bogus 0.
    let mut name = String::new();
    let mut util: Option<f32> = None;
    let (mut used, mut total) = (None::<u64>, None::<u64>);
    let acc = |slot: &mut Option<u64>, s: &str| {
        if let Ok(v) = s.parse::<u64>() {
            *slot = Some(slot.unwrap_or(0) + v * 1_048_576); // MiB → B
        }
    };
    for line in text.lines() {
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if cols.len() < 4 {
            continue;
        }
        if name.is_empty() {
            name = cols[0].to_string();
        }
        if let Ok(v) = cols[1].parse::<f32>() {
            util = Some(util.map_or(v, |u| u.max(v)));
        }
        acc(&mut used, cols[2]);
        acc(&mut total, cols[3]);
    }
    (!name.is_empty()).then_some(GpuStats {
        name,
        util_pct: util,
        mem_used_bytes: used,
        mem_total_bytes: total,
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
    let (util, used, total) = probe_macos_gpu_perf().await;
    Some(GpuStats {
        name,
        util_pct: util,
        mem_used_bytes: used,
        mem_total_bytes: total,
    })
}

/// IOGPU PerformanceStatistics via ioreg — no root required on Apple Silicon.
/// Yields (device util %, in-use system memory B, allocated system memory B).
#[cfg(target_os = "macos")]
async fn probe_macos_gpu_perf() -> (Option<f32>, Option<u64>, Option<u64>) {
    let Ok(out) = tokio::process::Command::new("ioreg")
        .args(["-l", "-w0"])
        .output()
        .await
    else {
        return (None, None, None);
    };
    if !out.status.success() {
        return (None, None, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut util = None;
    let mut used = None;
    let mut total = None;
    for line in text.lines().filter(|l| l.contains("PerformanceStatistics")) {
        // Exact key match via closing quote + '=' — avoids e.g.
        // "In use system memory (driver)" shadowing the real key.
        let grab = |key: &str| -> Option<u64> {
            line.split_once(&format!("{key}\"="))?
                .1
                .split(|c: char| !(c.is_ascii_digit()))
                .find(|s| !s.is_empty())?
                .parse()
                .ok()
        };
        util = util.or_else(|| grab("Device Utilization %").map(|v| v as f32));
        used = used.or_else(|| grab("In use system memory"));
        total = total.or_else(|| grab("Alloc system memory"));
    }
    (util, used, total)
}

#[cfg(not(target_os = "macos"))]
async fn probe_macos_gpu() -> Option<GpuStats> {
    None
}
