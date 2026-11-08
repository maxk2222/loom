//! Shared configuration schemas (client TOML config, desired-state payload
//! pushed by the server, capacity models).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use crate::capability::{BackendType, Capabilities, SharingMode};

// ---------------------------------------------------------------------------
// Client local configuration (TOML file, editable via local admin API)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Human-readable workstation name shown in the dashboard.
    #[serde(default = "default_client_name")]
    pub name: String,

    /// Server base URL for the gRPC channel, e.g. https://loom.example.org:50051
    #[serde(default)]
    pub server_url: String,

    /// Connect to the central server at all. When false the workstation runs
    /// fully local — no gRPC session, no tier-2 sharing.
    #[serde(default = "default_true")]
    pub connect_enabled: bool,

    /// Device credential issued at enrollment.
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub device_credential: String,

    /// Priority (local) OpenAI API listener.
    #[serde(default)]
    pub local_api: LocalApiConfig,

    /// Inference backends and their deployments.
    #[serde(default)]
    pub backends: Vec<BackendConfig>,

    /// gRPC connector tuning.
    #[serde(default)]
    pub connector: ConnectorConfig,

    /// Whether this workstation participates in cluster sharing at all.
    #[serde(default)]
    pub sharing_enabled: bool,

    /// Local resource pressure limits: when system CPU/RAM exceed these,
    /// tier-2 admission gates close so local work always wins.
    #[serde(default)]
    pub pressure: PressureConfig,

    /// Updater.
    #[serde(default)]
    pub updater: UpdaterConfig,

    /// Desktop tray icon settings.
    #[serde(default)]
    pub tray: TrayConfig,

    /// Optional remote-config overrides pushed by the server (desired state).
    #[serde(default)]
    pub config_version: u64,
}

fn default_client_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "loom-client".into())
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            name: default_client_name(),
            server_url: String::new(),
            connect_enabled: true,
            client_id: String::new(),
            device_credential: String::new(),
            local_api: LocalApiConfig::default(),
            backends: Vec::new(),
            connector: ConnectorConfig::default(),
            sharing_enabled: false,
            pressure: PressureConfig::default(),
            updater: UpdaterConfig::default(),
            tray: TrayConfig::default(),
            config_version: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalApiConfig {
    /// Bind address for the priority API + local admin UI.
    #[serde(default = "default_local_bind")]
    pub bind: String,
    /// Optional bearer token required on local API (empty = open on loopback).
    #[serde(default)]
    pub api_key: String,
    /// Optional local UI user (first-run setup wizard). Argon2 hash, not
    /// plaintext — when set, the UI signs in with username+password and gets
    /// a session cookie instead of needing the raw api_key.
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub user_hash: String,
    /// Set once the first-run auth wizard was completed or skipped — keeps it
    /// from showing on every start while the API stays open.
    #[serde(default)]
    pub setup_done: bool,
    /// Serve the embedded minimal UI at /ui.
    #[serde(default = "default_true")]
    pub serve_ui: bool,
}

fn default_local_bind() -> String {
    "127.0.0.1:8391".into()
}
fn default_true() -> bool {
    true
}

impl Default for LocalApiConfig {
    fn default() -> Self {
        Self {
            bind: default_local_bind(),
            api_key: String::new(),
            user_name: String::new(),
            user_hash: String::new(),
            setup_done: false,
            serve_ui: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorConfig {
    /// Heartbeat interval in seconds.
    #[serde(default = "default_hb")]
    pub heartbeat_secs: u64,
    /// TLS mode for the gRPC channel.
    #[serde(default)]
    pub tls: TlsMode,
    /// Path to a CA bundle to verify the server (empty = system roots).
    #[serde(default)]
    pub ca_cert_path: String,
    /// Client certificate + key for mTLS — required when the server runs
    /// with grpc_tls.client_ca_path set. Empty = automatic PKI: the client
    /// enrolls/renews certs via the server's bootstrap listener.
    #[serde(default)]
    pub client_cert_path: String,
    #[serde(default)]
    pub client_key_path: String,
    /// TLS bootstrap endpoint for cert issuance/renewal (server-auth only).
    /// Empty = derive from server_url with port+1.
    #[serde(default)]
    pub bootstrap_url: String,
}

fn default_hb() -> u64 {
    10
}

impl Default for ConnectorConfig {
    fn default() -> Self {
        Self {
            heartbeat_secs: default_hb(),
            tls: TlsMode::Auto,
            ca_cert_path: String::new(),
            client_cert_path: String::new(),
            client_key_path: String::new(),
            bootstrap_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// TLS when URL scheme is https/grpcs, plaintext for http/grpc (dev).
    #[default]
    Auto,
    /// Always TLS.
    Require,
    /// Plaintext (dev/lab only).
    Off,
}

/// Local resource pressure thresholds. Disabled when `enabled` is false or
/// both percentages are 0. When breached, every non-private deployment's
/// tier-2 gate closes (`accepting` flips off so the server drains away) and
/// — with `unload_models` — idle deployments also unload their model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// System CPU usage % that triggers the gate (0 = ignore CPU).
    #[serde(default)]
    pub cpu_pct: u8,
    /// System RAM usage % that triggers the gate (0 = ignore RAM).
    #[serde(default)]
    pub ram_pct: u8,
    /// Also unload models on idle deployments while the gate is closed.
    /// auto_load deployments re-warm automatically once pressure clears.
    #[serde(default)]
    pub unload_models: bool,
    /// Seconds pressure must stay below limits before gates reopen
    /// (hysteresis so the gate does not flap on noisy load).
    #[serde(default = "default_pressure_cooldown")]
    pub cooldown_secs: u64,
    /// GPU VRAM usage % above which `auto_load` skips warming — a deployment
    /// whose model would not fit anyway must not even try (0 = ignore).
    /// Not a tier-2 gate: only the warm-up path consults this.
    #[serde(default)]
    pub gpu_vram_pct: u8,
}

fn default_pressure_cooldown() -> u64 {
    30
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cpu_pct: 0,
            ram_pct: 0,
            unload_models: false,
            cooldown_secs: default_pressure_cooldown(),
            gpu_vram_pct: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "stable" | "beta"
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Automatically apply updates when found (default: manual).
    #[serde(default)]
    pub auto_apply: bool,
    /// minisign public key (base64) used to verify release artifacts.
    /// Empty = built-in LOOM release key; set to pin your own signing key.
    #[serde(default)]
    pub public_key: String,
    /// HTTP base of the server's dashboard/API used to fetch the release
    /// manifest. Empty = derived from `server_url` (grpc port → http port).
    #[serde(default)]
    pub server_base: String,
}

fn default_channel() -> String {
    "stable".into()
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: default_channel(),
            auto_apply: false,
            public_key: String::new(),
            server_base: String::new(),
        }
    }
}

/// One inference backend endpoint attached to this workstation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Unique id — stable across restarts; generated once.
    pub id: String,
    /// Friendly name (unique per client).
    pub name: String,
    pub backend_type: BackendType,
    /// Base URL of the backend, e.g. http://127.0.0.1:11434
    pub endpoint: String,
    /// Optional API key for the backend itself.
    #[serde(default)]
    pub api_key: String,
    /// Deployments exposed through this backend.
    #[serde(default)]
    pub deployments: Vec<DeploymentConfig>,
    /// Enabled toggle.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hosted/remote provider (OpenRouter, Groq, Azure, Anthropic, …): no own
    /// compute — auto-load warm-up and JIT keep-warm are skipped.
    #[serde(default)]
    pub remote: bool,
    /// Azure OpenAI only: api-version query param.
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// Health probe interval seconds.
    #[serde(default = "default_health")]
    pub health_interval_secs: u64,
}

fn default_api_version() -> String {
    "2024-10-21".into()
}

fn default_health() -> u64 {
    15
}

/// A deployment = one model exposed under a deployment id on the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentConfig {
    /// Unique deployment id (registered server-side).
    pub id: String,
    /// Friendly name.
    pub name: String,
    /// Physical model name understood by the backend (e.g. "llama3.1:8b").
    pub model_name: String,
    /// Capability overrides (merged over probed values).
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Manual overrides win over probing.
    #[serde(default)]
    pub capability_overrides: bool,
    /// Max concurrent requests on this deployment (all classes).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Slots reserved for local priority traffic.
    #[serde(default = "default_reserve")]
    pub local_reserve: u32,
    /// Max concurrent tier-2 requests.
    #[serde(default = "default_tier2")]
    pub tier2_max: u32,
    /// Sharing mode toward the cluster.
    #[serde(default)]
    pub sharing_mode: SharingMode,
    /// Proactively load the model when the backend is healthy and the
    /// deployment is idle (JIT engines like Ollama/LM Studio).
    #[serde(default)]
    pub auto_load: bool,
    /// Enabled toggle.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_max_concurrent() -> u32 {
    4
}
fn default_reserve() -> u32 {
    1
}
fn default_tier2() -> u32 {
    2
}

// ---------------------------------------------------------------------------
// Desired state pushed from server (client-relevant config only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredClientConfig {
    /// Deployment-level desired values (keyed by deployment id).
    #[serde(default)]
    pub deployments: BTreeMap<String, DesiredDeployment>,
    /// Client-level desired values.
    #[serde(default)]
    pub client: DesiredClientFields,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredDeployment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier2_max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_reserve: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharing_mode: Option<SharingMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredClientFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharing_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    /// Updater directives pushed by the admin (roll-out a release).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updater: Option<DesiredUpdater>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredUpdater {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_apply: Option<bool>,
    /// Trigger an update check + apply right now (one-shot, not persisted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_now: Option<bool>,
}

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
    #[serde(default = "default_grpc_bind")]
    pub grpc_bind: String,
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default = "default_redis_url")]
    pub redis_url: String,
    /// Key prefix + environment namespace for Redis keys, e.g. "loom:prod:"
    #[serde(default = "default_redis_prefix")]
    pub redis_prefix: String,
    /// TLS config for the gRPC listener (optional; Caddy can terminate instead).
    #[serde(default)]
    pub grpc_tls: Option<GrpcTlsConfig>,
    /// Optional extra plaintext gRPC listener kept alive during a TLS
    /// migration — legacy clients reconnect here until they switch to TLS.
    #[serde(default)]
    pub grpc_plain_bind: Option<String>,
    /// Optional TLS bootstrap listener (server-auth only, no client cert):
    /// clients enroll/renew mTLS certs here via CSR exchange in Hello.
    #[serde(default)]
    pub grpc_bootstrap_bind: Option<String>,
    /// Directory for mutable data (logo uploads, release artifacts).
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Static dashboard dir (empty = embedded assets if built with them).
    #[serde(default)]
    pub dashboard_dir: String,
    /// Scheduler/queue runtime tunables (overridable via routing_policies).
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// Request metadata retention in days.
    #[serde(default = "default_retention")]
    pub history_retention_days: u32,
}

fn default_http_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_grpc_bind() -> String {
    "0.0.0.0:50051".into()
}
fn default_database_url() -> String {
    "postgres://loom:loom@localhost:5432/loom".into()
}
fn default_redis_url() -> String {
    "redis://localhost:6379/0".into()
}
fn default_redis_prefix() -> String {
    "loom:prod:".into()
}
fn default_data_dir() -> String {
    "./data".into()
}
fn default_retention() -> u32 {
    30
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_bind: default_http_bind(),
            grpc_bind: default_grpc_bind(),
            database_url: default_database_url(),
            redis_url: default_redis_url(),
            redis_prefix: default_redis_prefix(),
            grpc_tls: None,
            grpc_plain_bind: None,
            grpc_bootstrap_bind: None,
            data_dir: default_data_dir(),
            dashboard_dir: String::new(),
            scheduler: SchedulerConfig::default(),
            history_retention_days: default_retention(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcTlsConfig {
    pub cert_path: String,
    pub key_path: String,
    /// CA bundle that client certificates are verified against. When set,
    /// the listener requires mTLS — clients must present a cert signed by
    /// this CA. Device credentials still authenticate at the app layer.
    #[serde(default)]
    pub client_ca_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Global max queued requests across all classes.
    #[serde(default = "default_max_queue")]
    pub max_queue: usize,
    /// Max seconds a request may wait in queue.
    #[serde(default = "default_queue_timeout")]
    pub queue_timeout_secs: u64,
    /// Lease TTL seconds (reservation before assignment ack).
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl_secs: u64,
    /// Ack timeout for AssignRequest.
    #[serde(default = "default_ack_timeout")]
    pub assign_ack_timeout_ms: u64,
    /// Max candidate attempts per request.
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    /// Number of priority classes in the global queue.
    #[serde(default = "default_classes")]
    pub priority_classes: u32,
    /// Request deadline default (server-side hard cap).
    #[serde(default = "default_request_deadline")]
    pub request_deadline_secs: u64,
}

fn default_max_queue() -> usize {
    512
}
fn default_queue_timeout() -> u64 {
    120
}
fn default_lease_ttl() -> u64 {
    30
}
fn default_ack_timeout() -> u64 {
    3000
}
fn default_attempts() -> u32 {
    3
}
fn default_classes() -> u32 {
    4
}
fn default_request_deadline() -> u64 {
    600
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_queue: default_max_queue(),
            queue_timeout_secs: default_queue_timeout(),
            lease_ttl_secs: default_lease_ttl(),
            assign_ack_timeout_ms: default_ack_timeout(),
            max_attempts: default_attempts(),
            priority_classes: default_classes(),
            request_deadline_secs: default_request_deadline(),
        }
    }
}

/// Desktop tray icon settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TrayConfig {
    /// Show a system-tray icon (requires a desktop session).
    #[serde(default)]
    pub enabled: bool,
}
