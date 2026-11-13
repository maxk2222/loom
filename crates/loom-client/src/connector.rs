//! Connector: persistent client-initiated gRPC channel to the server.
//!
//! - Control stream (`Control`): hello/register, heartbeats, capacity/inventory
//!   reports, config push/ack, AssignRequest/Ack, CancelRequest.
//! - Per-request data streams (`RequestData`): opened by the client after
//!   accepting an assignment; request payload flows server->client, response
//!   bytes flow client->server.
//! - Reconnect with exponential backoff + session recovery + config full-sync.

use crate::adapters::AdapterError;
use crate::backend_manager::BackendManager;
use crate::config::ConfigStore;
use crate::scheduler::{LocalScheduler, RequestClass};
use crate::telemetry;
use crate::ws_agent;
use anyhow::Context;
use futures_util::StreamExt;
use loom_core::capability::ApiType;
use loom_core::config::{ClientConfig, ConnectorEndpoint, DesiredClientConfig, TlsMode};
use loom_protocol::limits;
use loom_protocol::pb::agent_client::AgentClient;
use loom_protocol::pb::*;
use loom_protocol::{client_msgs, frames, server_msgs};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::{debug, info, warn};

const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How a control session ended. `Prefer(i)` means a higher-priority
/// endpoint came back while we were on a fallback — reconnect there.
enum SessionEnd {
    Clean,
    Prefer(usize),
}

/// Transport behind one endpoint: tonic gRPC or the WebSocket bridge.
/// `request_data`/`get_config` funnel through it so the request runner
/// and config sync stay transport-agnostic.
#[derive(Clone)]
enum AgentTransport {
    Grpc(AgentClient<Channel>),
    Ws(Arc<ws_agent::WsAgent>),
}

impl AgentTransport {
    /// Open a per-request data stream; the caller's `out` queue carries
    /// Head/response frames, the returned stream yields server frames.
    async fn request_data(
        &self,
        out: mpsc::Receiver<DataFrame>,
    ) -> anyhow::Result<ws_agent::Inbound<DataFrame>> {
        match self {
            Self::Grpc(agent) => {
                let mut a = agent.clone();
                let resp = a.request_data(ReceiverStream::new(out)).await?;
                Ok(Box::pin(resp.into_inner()))
            }
            Self::Ws(agent) => agent.request_data(out).await,
        }
    }

    async fn get_config(&self, req: ConfigRequest) -> anyhow::Result<ConfigSnapshot> {
        match self {
            Self::Grpc(agent) => {
                let mut a = agent.clone();
                Ok(a.get_config(req).await?.into_inner())
            }
            Self::Ws(agent) => agent.get_config(req).await,
        }
    }
}

#[derive(Clone)]
pub struct Connector {
    state: Arc<ConnectorState>,
}

struct ActiveRequest {
    fencing: u64,
    cancel: CancellationToken,
}

pub struct ConnectorState {
    pub cfg_store: Arc<ConfigStore>,
    pub backends: BackendManager,
    pub scheduler: LocalScheduler,
    /// Set while a control session is live.
    pub connected: AtomicBool,
    pub session_id: RwLock<String>,
    pub server_version_hint: RwLock<String>,
    /// Notify -> debounced capacity report push.
    pub capacity_notify: std::sync::Arc<Notify>,
    /// Notify -> full inventory push (probe results, capability changes).
    pub inventory_notify: std::sync::Arc<Notify>,
    /// Notify -> drop the current session and reconnect (name/url/enabled
    /// changes take effect on the next hello).
    pub reconnect_notify: std::sync::Arc<Notify>,
    ctrl_tx: RwLock<Option<mpsc::Sender<ClientEnvelope>>>,
    agent: Mutex<Option<AgentClient<Channel>>>,
    active_requests: RwLock<HashMap<String, ActiveRequest>>,
    /// Latest host sample from sysmon — piggybacked on capacity reports.
    pub sys_stats: RwLock<crate::sysmon::SharedStats>,
    /// Shared finished-request history (wired to local_api::AppState).
    pub history: RwLock<Option<Arc<RwLock<std::collections::VecDeque<crate::local_api::FinReq>>>>>,
    /// Admin notices synced from the server (+ persistence path).
    pub notes: RwLock<Option<(crate::notes::SharedNotes, std::path::PathBuf)>>,
    /// CSR PEM to include in the next Hello (set when a cert is needed:
    /// first enroll or renewal). Consumed by build_hello.
    pub pending_csr: Mutex<Option<String>>,
    /// mTLS cert store (keypair + issued cert + expiry) next to config.
    pub cert_store: crate::tls_pki::CertStore,
    shutdown: CancellationToken,
}

impl Connector {
    pub fn new(
        cfg_store: Arc<ConfigStore>,
        backends: BackendManager,
        scheduler: LocalScheduler,
    ) -> Self {
        let cert_store = crate::tls_pki::CertStore::new(&cfg_store.dir().to_string_lossy())
            .or_else(|e| {
                // Under ProtectSystem=strict a non-writable config dir must not
                // panic the whole client — degrade to a temp dir (certs are
                // re-issued on restart; mTLS endpoints still work).
                tracing::warn!(error = %e, "cert store next to config not writable — using temp dir");
                crate::tls_pki::CertStore::new(&std::env::temp_dir().to_string_lossy())
            })
            .expect("cert store unusable in config dir and temp dir");
        Self {
            state: Arc::new(ConnectorState {
                cfg_store,
                backends,
                scheduler,
                connected: AtomicBool::new(false),
                session_id: RwLock::new(String::new()),
                server_version_hint: RwLock::new(String::new()),
                capacity_notify: std::sync::Arc::new(Notify::new()),
                inventory_notify: std::sync::Arc::new(Notify::new()),
                reconnect_notify: std::sync::Arc::new(Notify::new()),
                ctrl_tx: RwLock::new(None),
                agent: Mutex::new(None),
                active_requests: RwLock::new(HashMap::new()),
                sys_stats: RwLock::new(std::sync::Arc::new(RwLock::new(
                    crate::sysmon::SystemStats::default(),
                ))),
                history: RwLock::new(None),
                notes: RwLock::new(None),
                pending_csr: Mutex::new(None),
                cert_store,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Wire the sysmon stats handle so capacity reports carry host load.
    pub fn set_sys_stats(&self, s: crate::sysmon::SharedStats) {
        *self.state.sys_stats.write().unwrap() = s;
    }

    /// Wire the finished-request history ring shared with the local API.
    pub fn set_history(
        &self,
        h: Arc<RwLock<std::collections::VecDeque<crate::local_api::FinReq>>>,
    ) {
        *self.state.history.write().unwrap() = Some(h);
    }

    /// Wire the shared admin-notes store (+ its persistence path).
    pub fn set_notes(&self, n: crate::notes::SharedNotes, path: std::path::PathBuf) {
        *self.state.notes.write().unwrap() = Some((n, path));
    }

    pub fn state(&self) -> Arc<ConnectorState> {
        self.state.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.state.connected.load(Ordering::SeqCst)
    }

    /// Ask the connector to push a capacity report soon (debounced).
    pub fn notify_capacity(&self) {
        self.state.capacity_notify.notify_one();
    }

    /// Drop the active session and re-evaluate the config: reconnect when
    /// enabled, stay offline when `connect_enabled` turned off. Used after
    /// name / server_url / enabled changes so they take effect immediately.
    pub fn notify_reconnect(&self) {
        self.state.reconnect_notify.notify_one();
    }

    /// Abort a tier-2 request by request_id (admin/local cancel).
    pub fn abort_request(&self, request_id: &str) -> bool {
        if let Some(r) = self.state.active_requests.read().unwrap().get(request_id) {
            r.cancel.cancel();
            true
        } else {
            false
        }
    }

    pub fn shutdown(&self) {
        self.state.shutdown.cancel();
    }

    /// Main connector loop: connect → session → reconnect.
    pub async fn run(&self) {
        let mut backoff = Duration::from_secs(1);
        let mut tls_fail_streak = 0u32;
        // Debounced report tasks: capacity changes → capacity report;
        // probe/capability changes → full inventory (server needs caps for
        // routing filters — without this, probe results only arrived on
        // reconnect or config writes). Plus a periodic re-report: if the
        // server wiped runtime state (restart reconciliation) while this
        // control stream stayed up, nothing would ever re-trigger a report —
        // a steady 60s cadence heals the drift.
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut periodic = tokio::time::interval(Duration::from_secs(60));
            periodic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = periodic.tick() => {
                        state.send_capacity_report().await;
                    }
                    _ = state.capacity_notify.notified() => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        state.send_capacity_report().await;
                    }
                    _ = state.inventory_notify.notified() => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        state.push_inventory().await;
                    }
                }
            }
        });

        // Multi-endpoint: `connector.endpoints` is ordered by preference
        // (LAN/Tailscale first, public fallback last); `server_url` is the
        // implicit single entry. `ep_idx` walks the list on failures —
        // a clean end or a successful preferred-probe always resets to 0.
        let mut ep_idx = 0usize;
        loop {
            if self.state.shutdown.is_cancelled() {
                return;
            }
            let cfg = self.state.cfg_store.get().await;
            let eps = Self::endpoints(&cfg);
            if eps.is_empty() || !cfg.connect_enabled {
                self.set_disconnected().await;
                tokio::select! {
                    _ = self.state.shutdown.cancelled() => return,
                    _ = self.state.reconnect_notify.notified() => continue,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                }
            }
            let ep = eps[ep_idx.min(eps.len() - 1)].clone();
            let earlier = &eps[..ep_idx.min(eps.len() - 1)];
            let reconnect_now = tokio::select! {
                res = self.session(&cfg, &ep, earlier) => {
                    match res {
                        Ok(SessionEnd::Clean) => {
                            info!(url = %ep.url, "control session ended cleanly");
                            backoff = Duration::from_secs(1);
                            tls_fail_streak = 0;
                            ep_idx = 0;
                        }
                        Ok(SessionEnd::Prefer(i)) => {
                            info!(url = %eps[i].url, "preferred endpoint reachable — migrating");
                            ep_idx = i;
                            self.set_disconnected().await;
                            continue;
                        }
                        Err(e) => {
                            warn!(url = %ep.url, error = %format!("{e:#}"), "control session failed");
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                            // Rotate to the next endpoint on failure.
                            ep_idx = (ep_idx + 1) % eps.len();
                            // Auto-PKI resilience: a stored cert that no
                            // longer chains to the live CA (rotation/re-issue)
                            // stays "valid" for ~180d and would fail forever.
                            // After repeated transport failures, drop it —
                            // the next attempt re-bootstraps from the live CA.
                            // Only mTLS endpoints exercise the cert — don't
                            // burn the streak on credential-only fallbacks.
                            if ep.pki {
                                tls_fail_streak += 1;
                            }
                            if cfg.connector.client_cert_path.is_empty()
                                && tls_fail_streak >= 5
                            {
                                self.state.cert_store.clear_cert();
                                tls_fail_streak = 0;
                                warn!("dropping stored mTLS cert after repeated TLS failures — re-bootstrapping");
                            }
                        }
                    }
                    false
                }
                // Identity/connection settings changed → drop the session and
                // reconnect immediately with the fresh config.
                _ = self.state.reconnect_notify.notified() => {
                    ep_idx = 0;
                    true
                }
            };
            self.set_disconnected().await;
            if reconnect_now {
                backoff = Duration::from_secs(1);
                tls_fail_streak = 0;
                continue;
            }
            tokio::select! {
                _ = self.state.shutdown.cancelled() => return,
                _ = self.state.reconnect_notify.notified() => continue,
                _ = tokio::time::sleep(backoff) => {},
            }
        }
    }

    async fn set_disconnected(&self) {
        self.state.connected.store(false, Ordering::SeqCst);
        telemetry::server_connected(false);
        *self.state.ctrl_tx.write().unwrap() = None;
        *self.state.agent.lock().await = None;
        *self.state.session_id.write().unwrap() = String::new();
        // Cancel all in-flight tier-2 requests; their data streams are dead.
        let ids: Vec<String> = self
            .state
            .active_requests
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.abort_request(&id);
        }
    }

    async fn endpoint(&self, cfg: &ClientConfig) -> anyhow::Result<Endpoint> {
        // Identity: explicit paths take precedence; otherwise the auto-PKI
        // store supplies cert+key (issued via the bootstrap listener).
        let identity = if cfg.connector.client_cert_path.is_empty() {
            self.state.cert_store.handshake_material()
        } else {
            let cert =
                std::fs::read_to_string(&cfg.connector.client_cert_path).with_context(|| {
                    format!(
                        "reading connector.client_cert_path {}",
                        cfg.connector.client_cert_path
                    )
                })?;
            let key =
                std::fs::read_to_string(&cfg.connector.client_key_path).with_context(|| {
                    format!(
                        "reading connector.client_key_path {}",
                        cfg.connector.client_key_path
                    )
                })?;
            Some((cert, key))
        };
        // CA precedence: explicit ca_cert_path > pinned server CA > system roots.
        let ca_override = if cfg.connector.ca_cert_path.is_empty() {
            self.state.cert_store.saved_ca()
        } else {
            None
        };
        Self::endpoint_tls(cfg, &cfg.server_url.clone(), identity, ca_override).await
    }

    async fn endpoint_tls(
        cfg: &ClientConfig,
        raw_url: &str,
        identity: Option<(String, String)>,
        ca_override: Option<String>,
    ) -> anyhow::Result<Endpoint> {
        let mut url = raw_url.to_string();
        if !url.contains("://") {
            url = format!("https://{url}");
        }
        // TLS policy: Require upgrades any explicit http scheme, Off forces
        // plaintext (dev/lab), Auto honours the configured scheme.
        match cfg.connector.tls {
            TlsMode::Require => {
                if let Some(rest) = url.strip_prefix("http://") {
                    url = format!("https://{rest}");
                }
            }
            TlsMode::Off => {
                if let Some(rest) = url.strip_prefix("https://") {
                    url = format!("http://{rest}");
                }
            }
            TlsMode::Auto => {}
        }
        let mut ep = Endpoint::from_shared(url)?;
        ep = ep
            .connect_timeout(Duration::from_secs(10))
            // No per-RPC wall-clock timeout: the control + request-data
            // streams are long-lived by design, and a fixed cap would kill
            // active streams carrying multi-hour generations mid-flight.
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(20))
            .keep_alive_while_idle(true);
        if ep.uri().scheme_str() == Some("https") {
            // Provider ambiguity (aws-lc-rs + ring both in the dep tree) —
            // Once-guard covers tests and non-main entry points; main.rs
            // installs it eagerly too.
            static TLS_PROVIDER_INIT: std::sync::Once = std::sync::Once::new();
            TLS_PROVIDER_INIT.call_once(|| {
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            });
            let mut tls = ClientTlsConfig::new().with_enabled_roots();
            if !cfg.connector.ca_cert_path.is_empty() {
                let pem = std::fs::read(&cfg.connector.ca_cert_path).with_context(|| {
                    format!(
                        "reading connector.ca_cert_path {}",
                        cfg.connector.ca_cert_path
                    )
                })?;
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
            } else if let Some(ca_pem) = ca_override {
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(ca_pem));
            }
            if let Some((cert, key)) = identity {
                tls = tls.identity(tonic::transport::Identity::from_pem(cert, key));
            }
            ep = ep.tls_config(tls)?;
        }
        Ok(ep)
    }

    /// Cert issuance/renewal over the bootstrap listener (server-auth TLS,
    /// no client cert): send Hello with CSR, persist the returned cert + CA.
    async fn bootstrap_cert(&self, cfg: &ClientConfig) -> anyhow::Result<()> {
        let url = if !cfg.connector.bootstrap_url.is_empty() {
            cfg.connector.bootstrap_url.clone()
        } else {
            derive_bootstrap_url(&cfg.server_url)
        };
        let ca_override = if cfg.connector.ca_cert_path.is_empty() {
            self.state.cert_store.saved_ca()
        } else {
            None
        };
        let ep = Self::endpoint_tls(cfg, &url, None, ca_override).await?;
        let channel = ep.connect().await?;
        let mut agent = AgentClient::new(channel);
        let (tx, rx) = mpsc::channel::<ClientEnvelope>(4);
        let resp = agent.control(ReceiverStream::new(rx)).await?;
        let mut inbound = resp.into_inner();
        let cid = if cfg.client_id.is_empty() {
            "new"
        } else {
            &cfg.client_id
        };
        *self.state.pending_csr.lock().await = Some(self.state.cert_store.csr_pem(cid)?);
        let hello = self.build_hello(cfg).await;
        tx.send(ClientEnvelope {
            msg: Some(client_envelope::Msg::Hello(hello)),
        })
        .await?;
        let Some(Ok(ServerEnvelope {
            msg: Some(server_envelope::Msg::HelloAck(ack)),
        })) = inbound.next().await
        else {
            anyhow::bail!("bootstrap: no HelloAck");
        };
        if !ack.accepted {
            anyhow::bail!("bootstrap rejected: {}", ack.reject_reason);
        }
        self.persist_issued(&ack, cfg).await;
        if ack.client_cert_pem.is_empty() {
            anyhow::bail!("bootstrap: server issued no cert");
        }
        info!("mTLS cert issued/renewed via bootstrap listener");
        Ok(())
    }

    /// Save cert + CA + enrollment ids returned in HelloAck (any listener).
    async fn persist_issued(&self, ack: &loom_protocol::pb::HelloAck, cfg: &ClientConfig) {
        if !ack.client_cert_pem.is_empty() {
            if let Err(e) = self
                .state
                .cert_store
                .save(&ack.client_cert_pem, ack.client_cert_expires_unix)
            {
                warn!(error = %e, "saving issued client cert failed");
            }
        }
        if !ack.ca_cert_pem.is_empty() {
            let _ = self.state.cert_store.save_ca(&ack.ca_cert_pem);
        }
        if !ack.device_credential.is_empty() || ack.client_id != cfg.client_id {
            let _ = self
                .state
                .cfg_store
                .update(|c| {
                    if !ack.device_credential.is_empty() {
                        c.device_credential = ack.device_credential.clone();
                    }
                    if !ack.client_id.is_empty() {
                        c.client_id = ack.client_id.clone();
                    }
                })
                .await;
        }
    }

    /// Ordered effective endpoints: explicit `connector.endpoints`, else a
    /// single pki endpoint from `server_url` (classic behaviour).
    /// Every port-less `https://` endpoint additionally gets a `wss://`
    /// shadow at lowest priority: a 443 host without an explicit port is
    /// almost always edge-proxied, where buffered tunnels (cloudflared
    /// http2 without QUIC) can never finish a streaming gRPC Hello. The
    /// WS bridge on the same host is the fallback; on plain gRPC servers
    /// the extra dial just fails fast and rotation continues.
    fn endpoints(cfg: &ClientConfig) -> Vec<ConnectorEndpoint> {
        let mut eps = if !cfg.connector.endpoints.is_empty() {
            cfg.connector.endpoints.clone()
        } else if cfg.server_url.is_empty() {
            Vec::new()
        } else {
            vec![ConnectorEndpoint {
                url: cfg.server_url.clone(),
                pki: true,
            }]
        };
        let shadows: Vec<ConnectorEndpoint> = eps
            .iter()
            .filter(|e| e.url.starts_with("https://"))
            .filter(|e| {
                url::Url::parse(&e.url)
                    .map(|u| u.port().is_none())
                    .unwrap_or(false)
            })
            .filter_map(|e| {
                let w = ws_agent::ws_url(&e.url);
                (!eps.iter().any(|x| norm_ep_url(&x.url) == norm_ep_url(&w)))
                    .then_some(ConnectorEndpoint { url: w, pki: e.pki })
            })
            .collect();
        eps.extend(shadows);
        eps
    }

    /// One control session against a specific endpoint. `earlier` lists the
    /// higher-priority endpoints — while connected on a fallback, a probe
    /// task watches them and ends the session early once one is reachable
    /// again so the loop can migrate back to the preferred path.
    async fn session(
        &self,
        cfg: &ClientConfig,
        ep: &ConnectorEndpoint,
        earlier: &[ConnectorEndpoint],
    ) -> anyhow::Result<SessionEnd> {
        // Own a mutable copy — a successful bootstrap may deliver a fresh
        // client_id + device_credential that the strict-port Hello needs.
        // server_url is pointed at the chosen endpoint so every downstream
        // consumer (endpoint_tls, derive_bootstrap_url, auto_pki scheme
        // check) follows the selected path transparently.
        let mut cfg = cfg.clone();
        cfg.server_url = ep.url.clone();
        // Auto-PKI: TLS + no manual cert paths → ensure a valid cert exists.
        // Skipped on pki=false endpoints (e.g. Cloudflare tunnel → plaintext
        // listener) — no cert bootstrap or CSR renewal against a path whose
        // port+1 bootstrap listener is not reachable. WebSocket endpoints
        // have no separate bootstrap listener at all: the CSR rides inside
        // Hello on the same channel, which is exactly what the listener's
        // cert-issuance path expects.
        let ws_ep = ws_agent::is_ws_url(&ep.url);
        let auto_pki = ep.pki
            && cfg.connector.tls != TlsMode::Off
            && cfg.connector.client_cert_path.is_empty()
            && match cfg.connector.tls {
                TlsMode::Require => true,
                _ => {
                    ws_ep || cfg.server_url.starts_with("https") || !cfg.server_url.contains("://")
                }
            };
        if auto_pki && self.state.cert_store.needs_renewal() {
            if ws_ep || self.state.cert_store.handshake_material().is_some() {
                // WS transport or expiring-but-valid cert: renew inline —
                // the CSR rides in Hello.
                let cid = if cfg.client_id.is_empty() {
                    "new".into()
                } else {
                    cfg.client_id.clone()
                };
                *self.state.pending_csr.lock().await = Some(self.state.cert_store.csr_pem(&cid)?);
            } else if let Err(e) = self.bootstrap_cert(&cfg).await {
                warn!(error = %format!("{e:#}"), "cert bootstrap failed — connecting without cert");
            } else {
                // Fresh enrollment may have assigned client_id/credential —
                // re-read before dialing the strict endpoint. The re-read
                // resets server_url — re-point it at the selected endpoint.
                cfg = self.state.cfg_store.get().await;
                cfg.server_url = ep.url.clone();
            }
        }
        let (mut inbound, transport, ctrl_tx) = self.open_control(&cfg, ep).await?;

        // Hello
        let hello = self.build_hello(&cfg).await;
        ctrl_tx
            .send(ClientEnvelope {
                msg: Some(client_envelope::Msg::Hello(hello)),
            })
            .await?;

        // Expect HelloAck
        let Some(Ok(ServerEnvelope {
            msg: Some(server_envelope::Msg::HelloAck(ack)),
        })) = inbound.next().await
        else {
            anyhow::bail!("server did not send HelloAck");
        };
        if !ack.accepted {
            anyhow::bail!("server rejected registration: {}", ack.reject_reason);
        }
        info!(session = %ack.session_id, client_id = %ack.client_id, "registered with server");

        // Persist assigned client id / device credential / issued cert.
        self.persist_issued(&ack, &cfg).await;

        // Server-pushed failover endpoints: enrolled-via-public clients
        // learn LAN/Tailscale endpoints automatically (merged, never
        // replacing configured entries — takes effect on next reconnect).
        self.apply_endpoint_hints(&ack.endpoint_hints).await;

        *self.state.session_id.write().unwrap() = ack.session_id.clone();
        *self.state.ctrl_tx.write().unwrap() = Some(ctrl_tx.clone());
        self.state.connected.store(true, Ordering::SeqCst);
        telemetry::server_connected(true);
        // Successful registration proves this boot healthy → disarm the
        // update watchdog (no-op when no update is pending).
        crate::updater::confirm_boot();

        // Config sync: if server's version differs, pull full snapshot.
        let local_version = self.state.cfg_store.get().await.config_version;
        if ack.config_version != local_version {
            debug!(
                local = local_version,
                remote = ack.config_version,
                "config version gap, pulling snapshot"
            );
            let cred = self.state.cfg_store.get().await.device_credential;
            let cid = self.state.cfg_store.get().await.client_id;
            match transport
                .get_config(ConfigRequest {
                    client_id: cid,
                    credential: cred,
                    last_config_version: local_version,
                })
                .await
            {
                Ok(snap) => {
                    if let Err(e) = self
                        .apply_desired(&snap.desired_config_json, snap.config_version)
                        .await
                    {
                        warn!(error = %e, "apply desired config failed");
                    }
                }
                Err(e) => warn!(error = %e, "GetConfig failed"),
            }
        }

        // Initial inventory + capacity report.
        let inv = self.build_inventory().await;
        let _ = ctrl_tx.send(client_msgs::inventory(inv)).await;
        self.state.send_capacity_report().await;

        // Heartbeat task. Session-scoped cancel: the guard fires when this
        // future returns *or* is dropped (reconnect_notify resolves the run()
        // select and drops us mid-await — exactly the `enabled=false` path).
        // A surviving hb task would hold a ctrl_tx clone, keep the
        // client→server stream open and the server-side session alive
        // (fresh last_hb → online) until the endpoint timeout.
        let session_cancel = CancellationToken::new();
        let _session_guard = session_cancel.clone().drop_guard();
        let hb_secs = ack.heartbeat_interval_secs.max(2) as u64;
        let hb_tx = ctrl_tx.clone();
        let session_id = ack.session_id.clone();
        let sched = self.state.scheduler.clone();
        let hb_cancel = session_cancel.clone();
        let shutdown = self.state.shutdown.clone();
        let hb_task = tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(hb_secs));
            loop {
                tokio::select! {
                    _ = t.tick() => {}
                    _ = hb_cancel.cancelled() => break,
                    _ = shutdown.cancelled() => break,
                }
                let (local, t2) = sched
                    .counters()
                    .iter()
                    .fold((0u32, 0u32), |a, c| (a.0 + c.1, a.1 + c.2));
                if hb_tx
                    .send(client_msgs::heartbeat(&session_id, local, t2))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Control message loop — while on a fallback endpoint a lightweight
        // probe re-dials the preferred endpoints every 45 s. The first that
        // completes the real (m)TLS handshake ends this session early; the
        // run loop then migrates there and the server kicks this session.
        // Skip probing an earlier gRPC endpoint that maps onto THIS ws
        // endpoint — it is the auto-shadow's own source. A plain connect()
        // succeeds on buffered tunnels (that's what makes them dangerous),
        // so without the filter the loop would endlessly migrate back to
        // the same broken gRPC path.
        let earlier_probe: Vec<(usize, ConnectorEndpoint)> = earlier
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !(ws_agent::is_ws_url(&ep.url)
                    && ws_agent::ws_url(&e.url) == ep.url.trim_end_matches('/'))
            })
            .map(|(i, e)| (i, e.clone()))
            .collect();
        let res = if earlier_probe.is_empty() {
            self.control_loop(&mut inbound, &ctrl_tx, &transport)
                .await
                .map(|_| SessionEnd::Clean)
        } else {
            let pcfg = cfg.clone();
            let pstate = self.state.clone();
            let preferred = earlier_probe.to_vec();
            let probe = async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(45)).await;
                    for (orig_i, e) in preferred.iter() {
                        let identity = if e.pki {
                            pstate.cert_store.handshake_material()
                        } else {
                            None
                        };
                        let ca = if pcfg.connector.ca_cert_path.is_empty() {
                            pstate.cert_store.saved_ca()
                        } else {
                            None
                        };
                        match Self::endpoint_tls(&pcfg, &e.url, identity, ca).await {
                            Ok(ep) => {
                                if ep.connect().await.is_ok() {
                                    return *orig_i;
                                }
                            }
                            Err(e2) => {
                                debug!(url = %e.url, error = %e2, "preferred-endpoint probe failed")
                            }
                        }
                    }
                }
            };
            tokio::select! {
                r = self.control_loop(&mut inbound, &ctrl_tx, &transport) => {
                    r.map(|_| SessionEnd::Clean)
                }
                i = probe => Ok(SessionEnd::Prefer(i)),
            }
        };
        hb_task.abort();
        res
    }

    async fn build_hello(&self, cfg: &ClientConfig) -> Hello {
        let inv = self.build_inventory().await;
        // Interface detection does a synchronous resolver per endpoint host —
        // keep it off the async task (a dead DNS would stall Hello).
        let cfg2 = cfg.clone();
        let addrs = tokio::task::spawn_blocking(move || Self::detect_interface_addrs(&cfg2))
            .await
            .unwrap_or_default();
        Hello {
            protocol_version: limits::PROTOCOL_VERSION,
            protocol_version_min: limits::PROTOCOL_VERSION_MIN,
            client_id: cfg.client_id.clone(),
            credential: if cfg.device_credential.is_empty() {
                // Enrollment phase: server_url config holds enroll token via
                // LOOM_ENROLL_TOKEN env or cli-provided credential.
                std::env::var("LOOM_ENROLL_TOKEN").unwrap_or_default()
            } else {
                cfg.device_credential.clone()
            },
            client_name: cfg.name.clone(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            session_resume_id: String::new(),
            last_config_version: cfg.config_version,
            inventory: Some(inv),
            platform: format!(
                "{}-{}{}",
                std::env::consts::OS,
                std::env::consts::ARCH,
                // Container clients self-update only when the exe runs from
                // a persistent writable volume (image entrypoint layout) —
                // the suffix tells the dashboard to offer the image-pull
                // path for deployments where it isn't.
                if crate::updater::in_container() && !crate::updater::can_self_update() {
                    "-container"
                } else {
                    ""
                }
            ),
            client_csr_pem: self
                .state
                .pending_csr
                .lock()
                .await
                .take()
                .unwrap_or_default(),
            ui_url: Self::advertise_ui_url(cfg, &addrs),
            addrs,
        }
    }

    /// Local admin UI URL advertised in Hello — the dashboard links it
    /// directly. Wildcard binds resolve to the first detected interface IP.
    /// Empty when the UI is disabled or loopback-only: a `127.0.0.1` link in
    /// an admin's browser would open *their* localhost, not the client's.
    fn advertise_ui_url(cfg: &ClientConfig, addrs: &[String]) -> String {
        if !cfg.local_api.serve_ui {
            return String::new();
        }
        let bind = cfg.local_api.bind.as_str();
        let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "8391"));
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let ip = if bare.is_empty() || bare == "0.0.0.0" || bare == "::" {
            match addrs.first() {
                Some(a) => a.as_str(),
                None => return String::new(), // wildcard but no usable IP
            }
        } else if bare == "127.0.0.1" || bare == "::1" || bare == "localhost" {
            return String::new();
        } else {
            bare
        };
        if ip.contains(':') {
            format!("http://[{ip}]:{port}")
        } else {
            format!("http://{ip}:{port}")
        }
    }

    /// Interface IPv4s the OS would pick to reach each configured endpoint
    /// (plus the default route): a UDP connect sends nothing — it only makes
    /// the kernel choose a source address. Dedup preserves probe order so a
    /// LAN/Tailscale path's IP lands ahead of the generic one.
    fn detect_interface_addrs(cfg: &ClientConfig) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut probe = |target: &str| {
            if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") {
                if s.connect(target).is_ok() {
                    if let Ok(a) = s.local_addr() {
                        let ip = a.ip().to_string();
                        if !a.ip().is_loopback() && !out.contains(&ip) {
                            out.push(ip);
                        }
                    }
                }
            }
        };
        for ep in Self::endpoints(cfg) {
            let h = url_host(&ep.url);
            if h.is_empty() {
                continue;
            }
            let target = if h.contains(':') {
                format!("[{h}]:443")
            } else {
                format!("{h}:443")
            };
            probe(&target);
        }
        // Default-route source IP catches machines where no endpoint resolves.
        probe("192.0.2.1:80");
        out
    }

    /// Merge admin-pushed endpoint hints into `connector.endpoints`:
    /// lan/wan-scoped hints lead (auto local-upgrade), configured entries
    /// keep their order and pki flags, public hints trail. Dedup by URL —
    /// a configured URL keeps its own pki flag even when promoted by a hint.
    async fn apply_endpoint_hints(&self, hints: &[EndpointHint]) {
        if hints.is_empty() {
            return;
        }
        let res = self
            .state
            .cfg_store
            .update(|cfg| {
                let merged = merge_endpoints(&Self::endpoints(cfg), hints);
                if merged == cfg.connector.endpoints {
                    return false;
                }
                cfg.connector.endpoints = merged;
                true
            })
            .await;
        match res {
            Ok(true) => info!("server endpoint hints merged into connector.endpoints"),
            Ok(false) => {}
            Err(e) => warn!(error = %e, "endpoint hint merge failed"),
        }
    }

    async fn build_inventory(&self) -> InventoryReport {
        InventoryReport {
            deployments: self.state.backends.inventory().await,
            capacities: self
                .state
                .scheduler
                .capacity_snapshot()
                .iter()
                .map(capacity_to_pb)
                .collect(),
            reported_at_unix_ms: server_msgs::now_ms().max(0) as u64,
        }
    }

    /// Open the control channel on the endpoint's transport: `ws(s)://`
    /// goes over the WebSocket bridge, everything else is tonic gRPC.
    /// Both yield the same normalized halves — a Status-errored inbound
    /// stream, a transport handle for data/config RPCs, and the outbound
    /// sender every control message flows through.
    async fn open_control(
        &self,
        cfg: &ClientConfig,
        ep: &ConnectorEndpoint,
    ) -> anyhow::Result<(
        ws_agent::Inbound<ServerEnvelope>,
        AgentTransport,
        mpsc::Sender<ClientEnvelope>,
    )> {
        if ws_agent::is_ws_url(&ep.url) {
            let agent = Arc::new(ws_agent::WsAgent::new(cfg, &ep.url)?);
            let (tx, inbound) = agent.control().await?;
            return Ok((inbound, AgentTransport::Ws(agent), tx));
        }
        let endpoint = self.endpoint(cfg).await?;
        if endpoint.uri().scheme_str() == Some("http") {
            let host = endpoint.uri().host().unwrap_or_default();
            if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
                warn!(
                    url = %ep.url,
                    "plaintext gRPC endpoint on a non-loopback host — device credential travels unencrypted (set connector.tls=\"require\" or https://)"
                );
            }
        }
        let channel = endpoint.connect().await?;
        let mut agent = AgentClient::new(channel);
        *self.state.agent.lock().await = Some(agent.clone());
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ClientEnvelope>(64);
        let response = agent.control(ReceiverStream::new(ctrl_rx)).await?;
        Ok((
            Box::pin(response.into_inner()),
            AgentTransport::Grpc(agent),
            ctrl_tx,
        ))
    }

    async fn control_loop(
        &self,
        inbound: &mut ws_agent::Inbound<ServerEnvelope>,
        ctrl_tx: &mpsc::Sender<ClientEnvelope>,
        agent: &AgentTransport,
    ) -> anyhow::Result<()> {
        while let Some(item) = inbound.next().await {
            let env = item?;
            let Some(msg) = env.msg else { continue };
            match msg {
                server_envelope::Msg::AssignRequest(a) => {
                    self.handle_assign(a, ctrl_tx.clone(), agent.clone());
                }
                server_envelope::Msg::CancelRequest(c) => {
                    debug!(request = %c.request_id, "cancel from server");
                    self.state
                        .abort_request_fenced(&c.request_id, c.fencing_token);
                }
                server_envelope::Msg::ConfigPush(p) => {
                    let applied = self
                        .apply_desired(&p.desired_config_json, p.config_version)
                        .await;
                    let _ = ctrl_tx
                        .send(client_msgs::config_ack(
                            p.config_version,
                            applied.is_ok(),
                            &applied.err().map(|e| e.to_string()).unwrap_or_default(),
                        ))
                        .await;
                }
                server_envelope::Msg::Ping(p) => {
                    let _ = ctrl_tx.send(client_msgs::pong(p.sent_at_unix_ms)).await;
                }
                server_envelope::Msg::NotesSync(sync) => {
                    let slot = self.state.notes.read().unwrap().clone();
                    if let Some((store, path)) = slot {
                        if crate::notes::apply_sync(&store, sync.notes) {
                            crate::notes::save_notes(&path, &store);
                            debug!("admin notes synced");
                        }
                    }
                }
                server_envelope::Msg::HelloAck(_) => {}
            }
        }
        anyhow::bail!("control stream closed by server")
    }

    /// Apply a desired-state config payload (from ConfigPush or GetConfig).
    async fn apply_desired(&self, payload: &[u8], version: u64) -> anyhow::Result<()> {
        let desired: DesiredClientConfig = serde_json::from_slice(payload)?;
        self.state
            .cfg_store
            .update(|c| {
                if let Some(v) = desired.client.sharing_enabled {
                    c.sharing_enabled = v;
                }
                for (dep_id, dd) in &desired.deployments {
                    // `removed` deletes the deployment from client config
                    // entirely — subsequent inventory reports no longer carry
                    // it, so the server-side row stays deleted.
                    if dd.removed == Some(true) {
                        for b in &mut c.backends {
                            b.deployments.retain(|d| &d.id != dep_id);
                        }
                        continue;
                    }
                    for b in &mut c.backends {
                        for d in &mut b.deployments {
                            if &d.id == dep_id {
                                if let Some(v) = dd.tier2_max {
                                    d.tier2_max = v;
                                }
                                if let Some(v) = dd.local_reserve {
                                    d.local_reserve = v;
                                }
                                if let Some(m) = dd.sharing_mode {
                                    d.sharing_mode = m;
                                }
                                if let Some(e) = dd.enabled {
                                    d.enabled = e;
                                }
                            }
                        }
                    }
                }
                c.config_version = version;
            })
            .await?;

        // Apply to live scheduler.
        let cfg = self.state.cfg_store.get().await;
        self.state
            .scheduler
            .set_sharing_enabled(cfg.sharing_enabled);
        for (dep_id, dd) in &desired.deployments {
            if dd.removed == Some(true) {
                continue;
            }
            self.state.scheduler.update_limits(dep_id, dd);
        }
        if desired
            .deployments
            .values()
            .any(|dd| dd.removed == Some(true))
        {
            let deps: Vec<_> = cfg
                .backends
                .iter()
                .flat_map(|b| b.deployments.clone())
                .collect();
            self.state.scheduler.sync_deployments(&deps);
            self.state.backends.sync_from_config(&cfg).await;
            self.state.push_inventory().await;
        }
        self.notify_capacity();

        // Updater directives: persist enabled/auto_apply/server_base, and
        // fire a one-shot update run when the admin pushed `update_now`.
        if let Some(up) = &desired.client.updater {
            if up.enabled.is_some() || up.auto_apply.is_some() || up.server_base.is_some() {
                let enabled = up.enabled;
                let auto_apply = up.auto_apply;
                let server_base = up.server_base.clone();
                self.state
                    .cfg_store
                    .update(|c| {
                        if let Some(v) = enabled {
                            c.updater.enabled = v;
                        }
                        if let Some(v) = auto_apply {
                            c.updater.auto_apply = v;
                        }
                        if let Some(v) = server_base {
                            c.updater.server_base = v;
                        }
                    })
                    .await?;
            }
            if up.update_now == Some(true) {
                let cfg_store = self.state.cfg_store.clone();
                tokio::spawn(async move {
                    let cfg = cfg_store.get().await;
                    let dir = cfg_store
                        .path()
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    match crate::updater::run_once(
                        &cfg.updater,
                        cfg.primary_server_url(),
                        env!("CARGO_PKG_VERSION"),
                        &dir,
                        true,
                    )
                    .await
                    {
                        Ok(st) => tracing::info!(?st, "admin-triggered update check done"),
                        Err(e) => tracing::warn!(error = %e, "admin-triggered update failed"),
                    }
                });
            }
        }
        Ok(())
    }

    fn handle_assign(
        &self,
        a: AssignRequest,
        ctrl_tx: mpsc::Sender<ClientEnvelope>,
        agent: AgentTransport,
    ) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let request_id = a.request_id.clone();
            // Reject api types this build doesn't understand *before*
            // accepting — retryable, so the server tries the next candidate
            // instead of failing the request on this client.
            if a.api_type.parse::<ApiType>().is_err() {
                let _ = ctrl_tx
                    .send(client_msgs::assign_reject(
                        &request_id,
                        &a.lease_id,
                        a.fencing_token,
                        "unsupported_api",
                        true,
                    ))
                    .await;
                return;
            }
            // Duplicate assign for an already-running request_id: accepting
            // would overwrite the live entry (its cancel handle is lost and
            // bookkeeping double-finishes). Reject retryable — the server
            // can route it elsewhere or drop it.
            if state
                .active_requests
                .read()
                .unwrap()
                .contains_key(&request_id)
            {
                let _ = ctrl_tx
                    .send(client_msgs::assign_reject(
                        &request_id,
                        &a.lease_id,
                        a.fencing_token,
                        "duplicate_assign",
                        true,
                    ))
                    .await;
                return;
            }
            // Admission check.
            let admit = state.scheduler.try_admit(
                &a.deployment_id,
                RequestClass::Tier2,
                &request_id,
                a.footprint_tokens,
            );

            match admit {
                Ok(token) => {
                    let cancel = token.cancel_token();
                    state.active_requests.write().unwrap().insert(
                        request_id.clone(),
                        ActiveRequest {
                            fencing: a.fencing_token,
                            cancel: cancel.clone(),
                        },
                    );
                    if ctrl_tx
                        .send(client_msgs::assign_accept(
                            &request_id,
                            &a.lease_id,
                            a.fencing_token,
                        ))
                        .await
                        .is_err()
                    {
                        state.scheduler.finish_request(&request_id);
                        return;
                    }
                    state.notify_capacity_now();
                    // Open the request data stream.
                    run_request(state.clone(), agent.clone(), a, token, ctrl_tx).await;
                }
                Err(r) => {
                    // Transient rejections let the server retry/defer —
                    // kv_full sits in the same class as no_capacity: the
                    // budget frees as running requests finish.
                    let retryable = matches!(
                        r,
                        crate::scheduler::RejectReason::NoCapacity
                            | crate::scheduler::RejectReason::AdmissionClosed
                            | crate::scheduler::RejectReason::KvFull
                    );
                    let _ = ctrl_tx
                        .send(client_msgs::assign_reject(
                            &request_id,
                            &a.lease_id,
                            a.fencing_token,
                            r.as_str(),
                            retryable,
                        ))
                        .await;
                }
            }
        });
    }
}

impl ConnectorState {
    /// Push a fresh inventory report (deployments + capacities) when config
    /// changed at runtime — without this the server only learns about new
    /// backends on the next reconnect.
    pub async fn push_inventory(&self) {
        let tx = self.ctrl_tx.read().unwrap().clone();
        if let Some(tx) = tx {
            let inv = InventoryReport {
                deployments: self.backends.inventory().await,
                capacities: self
                    .scheduler
                    .capacity_snapshot()
                    .iter()
                    .map(capacity_to_pb)
                    .collect(),
                reported_at_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            let _ = tx.send(client_msgs::inventory(inv)).await;
        }
    }

    /// Send a capacity report if connected.
    pub async fn send_capacity_report(&self) {
        let tx = self.ctrl_tx.read().unwrap().clone();
        if let Some(tx) = tx {
            let caps: Vec<DeploymentCapacity> = self
                .scheduler
                .capacity_snapshot()
                .iter()
                .map(capacity_to_pb)
                .collect();
            let host = {
                let arc = self.sys_stats.read().unwrap().clone();
                let s = arc.read().unwrap().clone();
                Some(loom_protocol::pb::HostStats {
                    cpu_pct: s.cpu_pct,
                    ram_pct: s.ram_pct,
                    ram_used_bytes: s.ram_used_bytes,
                    ram_total_bytes: s.ram_total_bytes,
                    pressure: s.pressure,
                    has_gpu: s.gpu.is_some(),
                    gpu_name: s.gpu.as_ref().map(|g| g.name.clone()).unwrap_or_default(),
                    gpu_util_pct: s.gpu.as_ref().and_then(|g| g.util_pct).unwrap_or(0.0),
                    gpu_mem_used_bytes: s.gpu.as_ref().and_then(|g| g.mem_used_bytes).unwrap_or(0),
                    gpu_mem_total_bytes: s
                        .gpu
                        .as_ref()
                        .and_then(|g| g.mem_total_bytes)
                        .unwrap_or(0),
                })
            };
            let _ = tx.send(client_msgs::capacity_report(caps, host)).await;
        }
    }

    fn notify_capacity_now(&self) {
        self.capacity_notify.notify_one();
    }

    /// Cancel only when the fencing token matches the active request — guards
    /// against stale cancels hitting a newer request with a recycled id.
    fn abort_request_fenced(&self, request_id: &str, fencing: u64) {
        if let Some(r) = self.active_requests.read().unwrap().get(request_id) {
            if r.fencing == fencing {
                r.cancel.cancel();
            }
        }
    }
}

fn capacity_to_pb(c: &crate::scheduler::DeploymentCapacityInfo) -> DeploymentCapacity {
    DeploymentCapacity {
        deployment_id: c.deployment_id.clone(),
        tier2_total: c.tier2_total,
        tier2_free: c.tier2_free,
        inflight_local: c.inflight_local,
        inflight_tier2: c.inflight_tier2,
        sharing_mode: c.sharing_mode.as_str().into(),
        accepting: c.accepting,
        pressure_gated: c.pressure_gated,
        health: c.health.into(),
        kv_budget_tokens: c.kv_budget_tokens,
        kv_used_tokens: c.kv_used_tokens,
    }
}

/// Execute one assigned inference request over a dedicated data stream.
async fn run_request(
    state: Arc<ConnectorState>,
    agent: AgentTransport,
    a: AssignRequest,
    token: crate::scheduler::AdmitToken,
    ctrl_tx: mpsc::Sender<ClientEnvelope>,
) {
    let request_id = a.request_id.clone();
    let lease_id = a.lease_id.clone();
    let fencing = a.fencing_token;
    let cancel = token.cancel_token();
    let started = std::time::Instant::now();
    telemetry::req_admitted("tier2");

    let (data_tx, data_rx) = mpsc::channel::<DataFrame>(32);
    // Head BEFORE the RPC call: the server reads it to correlate the stream
    // before sending response headers — sending it after `request_data().await`
    // would deadlock both sides.
    let head_res = data_tx
        .send(frames::head(&request_id, &lease_id, fencing))
        .await;
    let mut inbound = match head_res {
        Err(_) => None,
        Ok(()) => match agent.request_data(data_rx).await {
            Ok(r) => Some(r),
            Err(e) => {
                warn!(req = %request_id, error = %e, "open data stream failed");
                None
            }
        },
    };
    let mut inbound_ref = inbound.as_mut();
    let result = async {
        let inbound = inbound_ref.as_mut().ok_or("open data stream")?;

        // Assemble request payload from request_json frames.
        let mut payload = Vec::with_capacity(a.payload_bytes as usize);
        let payload_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while (payload.len() as u32) < a.payload_bytes {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Err("cancelled".to_string()),
                _ = tokio::time::sleep_until(payload_deadline) => {
                    return Err("payload timeout".to_string())
                }
                v = inbound.next() => v,
            };
            match next {
                Some(Ok(DataFrame {
                    frame: Some(data_frame::Frame::RequestJson(chunk)),
                })) => payload.extend_from_slice(&chunk),
                Some(Ok(DataFrame {
                    frame: Some(data_frame::Frame::Error(e)),
                })) => return Err(format!("server error: {}", e.code)),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(format!("stream error: {e}")),
                None => return Err("payload stream closed early".to_string()),
            }
        }
        if payload.is_empty() {
            return Err("empty request payload".to_string());
        }

        // Unknown api types are rejected — routing them as chat would send a
        // media request body to /v1/chat/completions. The server then tries
        // the next candidate (Reject is the graceful "not me" path).
        let api: ApiType = a
            .api_type
            .parse()
            .map_err(|_| format!("unknown api type '{}'", a.api_type))?;

        // Multipart bodies (audio transcription/translation) skip JSON
        // parsing and go through the raw verbatim forward path.
        let resp = if api.is_multipart() || a.content_type.starts_with("multipart/") {
            state
                .backends
                .forward_multipart(
                    &a.deployment_id,
                    api,
                    &payload,
                    &a.content_type,
                    cancel.clone(),
                )
                .await
                .map_err(|e| format!("upstream: {e}"))?
        } else {
            let body: serde_json::Value =
                serde_json::from_slice(&payload).map_err(|e| format!("bad payload json: {e}"))?;
            state
                .backends
                .forward(&a.deployment_id, api, body, cancel.clone())
                .await
                .map_err(|e| format!("upstream: {e}"))?
        };

        let upstream_status = u32::from(resp.status);
        let mut ttft_ms: i64 = 0;
        let mut usage = loom_core::openai::Usage::default();
        let mut upstream_err: Option<String> = None;
        let mut finish_reason = String::new();
        let mut saw_output = false;
        let mut stream = resp.stream;
        let mut last_usage_sent = 0u64;

        // Bridge upstream bytes -> response_chunk frames, until End.
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    // Terminal frame: fire-and-forget. A stopped server must
                    // never wedge the send — cleanup below has to run.
                    let _ = data_tx.try_send(frames::end(true, "cancelled", upstream_status));
                    return Err("cancelled".to_string());
                }
                v = stream.next() => v,
            };
            match next {
                Some(Ok(bytes)) => {
                    if ttft_ms == 0 {
                        ttft_ms = started.elapsed().as_millis() as i64;
                        telemetry::ttft(started.elapsed().as_secs_f64());
                    }
                    debug!(req = %request_id, n = bytes.len(), "upstream chunk → data stream");
                    // Opportunistically parse usage/finish from SSE tail.
                    inspect_sse_tail(&bytes, &mut usage, &mut finish_reason, &mut saw_output);
                    // Converge the server's token reservation toward the
                    // backend's real cumulative usage — long generations
                    // outgrow the submit-time estimate.
                    let total = u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens);
                    if total > last_usage_sent {
                        last_usage_sent = total;
                        let _ = ctrl_tx
                            .send(client_msgs::lease_usage(&request_id, total))
                            .await;
                    }
                    // A send blocked on a full channel must not wedge the task:
                    // when the server stops draining, cancellation still wins
                    // and the cleanup tail (slot + KV release) always runs.
                    let send = data_tx.send(frames::response_chunk(bytes.to_vec()));
                    tokio::pin!(send);
                    let sent = tokio::select! {
                        _ = cancel.cancelled() => {
                            let _ = data_tx.try_send(frames::end(true, "cancelled", upstream_status));
                            return Err("cancelled".to_string());
                        }
                        r = &mut send => r,
                    };
                    if sent.is_err() {
                        return Err("data stream closed".to_string());
                    }
                }
                Some(Err(e)) => {
                    // Mid-generation crash — counts toward the breaker just
                    // like a failed forward() call.
                    state.backends.record_failure(&a.deployment_id, &e);
                    upstream_err = Some(e.to_string());
                    break;
                }
                None => break,
            }
        }

        // A wedged backend can answer 200 with zero tokens and no content —
        // count that zombie response as a crash so the breaker still trips.
        if upstream_err.is_none() && !cancel.is_cancelled() {
            if let Some(e) = empty_generation_failure(
                api.is_text_family(),
                upstream_status as u16,
                saw_output,
                &usage,
            ) {
                state.backends.record_failure(&a.deployment_id, &e);
                upstream_err = Some(e.to_string());
            }
        }
        let status = if cancel.is_cancelled() {
            "cancelled"
        } else if upstream_err.is_some() || upstream_status >= 400 {
            // A proxied error body (5xx crash, 4xx rejection) is a failed
            // generation even though transport completed.
            "failed"
        } else {
            "done"
        };
        if upstream_err.is_none() && upstream_status >= 400 {
            upstream_err = Some(format!("upstream status {upstream_status}"));
        }
        // A clean stream end only counts as success when the upstream call
        // itself succeeded — status is already "failed" for proxied error
        // bodies, which the breaker counted at forward().
        if status == "done" {
            state.backends.record_success(&a.deployment_id);
        }
        // Terminal frame — fire-and-forget; the cleanup tail must never block
        // on a channel the server stopped draining.
        let _ = data_tx.try_send(frames::end(
            status == "cancelled",
            &finish_reason,
            upstream_status,
        ));
        Ok::<_, String>((status.to_string(), upstream_err, usage, ttft_ms))
    }
    .await;

    let (status, err, usage, ttft_ms) = match result {
        Ok(v) => v,
        Err(e) => {
            if e != "cancelled" {
                warn!(req = %request_id, error = %e, "tier2 request failed");
                let _ = data_tx.try_send(frames::error("upstream_error", &e, false));
            }
            (
                if e == "cancelled" {
                    "cancelled"
                } else {
                    "failed"
                }
                .to_string(),
                Some(e),
                loom_core::openai::Usage::default(),
                0,
            )
        }
    };

    // Flush: half-close the request stream, then drain the server's response
    // stream so all queued frames are consumed BEFORE the RPC object drops.
    // Dropping it earlier issues RST_STREAM and silently discards our final
    // response_chunk/end frames — the root cause of empty bodies on fast
    // (non-streaming) backends.
    drop(data_tx);
    if let Some(inbound) = inbound.as_mut() {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while inbound.next().await.is_some() {}
        })
        .await;
    }

    // Bounded: a wedged control channel must not block slot/KV release below.
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        ctrl_tx.send(client_msgs::request_end(
            &request_id,
            &lease_id,
            &status,
            &err.unwrap_or_default(),
            usage.prompt_tokens,
            usage.completion_tokens,
            ttft_ms,
            started.elapsed().as_millis() as i64,
        )),
    )
    .await;

    telemetry::req_finished(&status);
    telemetry::req_duration(started.elapsed().as_secs_f64(), "tier2");
    // Mirror into the shared request history (local UI charts include tier-2).
    if let Some(h) = state.history.read().unwrap().as_ref() {
        let mut h = h.write().unwrap();
        if h.len() >= 300 {
            h.pop_front();
        }
        h.push_back(crate::local_api::FinReq {
            class: "tier-2".into(),
            api: a.api_type.clone(),
            deployment_id: a.deployment_id.clone(),
            duration_ms: started.elapsed().as_millis() as i64,
            ttft_ms,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            status: status.clone(),
            ended_unix_ms: crate::local_api::now_ms(),
        });
    }
    state.scheduler.finish_request(&request_id);
    state.active_requests.write().unwrap().remove(&request_id);
    state.notify_capacity_now();
    drop(token);
}

/// Parse trailing SSE data for usage + finish_reason (best effort).
/// `saw_output` is set once any parsed payload carries generated content —
/// used to detect zombie backends that answer 200 with zero tokens.
pub(crate) fn inspect_sse_tail(
    bytes: &bytes::Bytes,
    usage: &mut loom_core::openai::Usage,
    finish_reason: &mut String,
    saw_output: &mut bool,
) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    // Non-streaming responses arrive as one raw JSON object — parse it whole.
    let trimmed = text.trim();
    if trimmed.starts_with('{') && !trimmed.contains("\ndata:") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(u) = v.get("usage") {
                if let Ok(parsed) = serde_json::from_value::<loom_core::openai::Usage>(u.clone()) {
                    *usage = parsed;
                }
            }
            if let Some(fr) = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("finish_reason"))
                .and_then(|f| f.as_str())
            {
                *finish_reason = fr.to_string();
            }
            if json_has_output(&v) {
                *saw_output = true;
            }
        }
        return;
    }
    for line in text.split("\n\n") {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(u) = v.get("usage") {
            if let Ok(parsed) = serde_json::from_value::<loom_core::openai::Usage>(u.clone()) {
                *usage = parsed;
            }
        }
        if let Some(fr) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
        {
            *finish_reason = fr.to_string();
        }
        if json_has_output(&v) {
            *saw_output = true;
        }
    }
}

/// True when a response/chunk carries any generated payload — chat/completion
/// text, tool calls, reasoning, embeddings data or transcription text.
pub(crate) fn json_has_output(v: &serde_json::Value) -> bool {
    let ne = |x: &serde_json::Value| x.as_str().is_some_and(|s| !s.is_empty());
    if ne(&v["text"]) || ne(&v["response"]) {
        return true;
    }
    if v["data"].as_array().is_some_and(|a| !a.is_empty()) {
        return true;
    }
    let Some(choices) = v["choices"].as_array() else {
        return false;
    };
    choices.iter().any(|c| {
        ne(&c["text"])
            || [&c["message"], &c["delta"]].iter().any(|h| {
                ne(&h["content"])
                    || ne(&h["reasoning_content"])
                    || h["tool_calls"].as_array().is_some_and(|a| !a.is_empty())
                    || h["audio"].is_object()
            })
    })
}

/// A 200 text-family response that produced zero tokens and no content comes
/// from a wedged generation pipeline (loaded model, dead runner) — it counts
/// as a crash-classified failure for the recovery breaker.
pub(crate) fn empty_generation_failure(
    api_text: bool,
    upstream_status: u16,
    saw_output: bool,
    usage: &loom_core::openai::Usage,
) -> Option<AdapterError> {
    if api_text
        && upstream_status < 400
        && !saw_output
        && usage.prompt_tokens == 0
        && usage.completion_tokens == 0
    {
        Some(AdapterError::Io(
            "empty generation: 200 with 0 tokens and no content".into(),
        ))
    } else {
        None
    }
}

/// Bootstrap (cert-issuance) URL derivation. Only meaningful on the
/// standard gRPC listener set — agent :50051/:50052/:50054 pairs with the
/// bootstrap listener on :50053. Port-less URLs and any other explicit
/// port (custom gateway port, plain HTTPS on :443) keep the same URL:
/// there is no bootstrap convention there, and guessing a neighbour port
/// dials an edge port that either does not exist or serves a different
/// protocol. The returned-same URL is still a valid fallback since the
/// bootstrap service is the same AgentSvc on server-auth TLS — a single
/// listener or an edge-proxied hostname can serve cert issuance directly.
fn derive_bootstrap_url(server_url: &str) -> String {
    let u = server_url.trim_end_matches('/');
    if let Some(idx) = u.rfind(':') {
        if let Ok(port) = u[idx + 1..].parse::<u16>() {
            if (50051..=50054).contains(&port) {
                return format!("{}:50053", &u[..idx]);
            }
        }
    }
    u.to_string()
}

/// Endpoint URLs dedup on the normalized form (trailing slash ignored).
fn norm_ep_url(u: &str) -> &str {
    u.trim_end_matches('/')
}

/// Merge admin-pushed EndpointHints into an ordered endpoint list:
/// lan/wan-scoped hints lead (auto local-upgrade), configured entries keep
/// their relative order, public hints trail. Dedup by normalized URL — a
/// URL present in `configured` keeps its own entry (and pki flag) even when
/// a hint promotes its position.
fn merge_endpoints(
    configured: &[ConnectorEndpoint],
    hints: &[EndpointHint],
) -> Vec<ConnectorEndpoint> {
    let mut out: Vec<ConnectorEndpoint> = Vec::new();
    let mut push = |ep: ConnectorEndpoint| {
        if !out
            .iter()
            .any(|e| norm_ep_url(&e.url) == norm_ep_url(&ep.url))
        {
            out.push(ep);
        }
    };
    for h in hints.iter().filter(|h| h.scope != "public") {
        let ep = configured
            .iter()
            .find(|e| norm_ep_url(&e.url) == norm_ep_url(&h.url))
            .cloned()
            .unwrap_or(ConnectorEndpoint {
                url: h.url.clone(),
                pki: h.pki,
            });
        push(ep);
    }
    for e in configured {
        push(e.clone());
    }
    for h in hints.iter().filter(|h| h.scope == "public") {
        push(ConnectorEndpoint {
            url: h.url.clone(),
            pki: h.pki,
        });
    }
    out
}

/// Host portion of an endpoint URL without pulling in a URL crate:
/// `https://host:50052/path` → `host`, `[::1]:50052` → `::1`,
/// `https://u:p@host` → `host`.
fn url_host(url: &str) -> &str {
    let s = url.split("://").nth(1).unwrap_or(url);
    let s = s.split('/').next().unwrap_or(s);
    let s = s.rsplit('@').next().unwrap_or(s);
    if let Some(rest) = s.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(s);
    }
    s.split(':').next().unwrap_or(s)
}

#[cfg(test)]
mod tls_policy_tests {
    use super::*;
    use loom_core::config::ConnectorConfig;

    fn cfg(url: &str, tls: TlsMode) -> ClientConfig {
        ClientConfig {
            server_url: url.into(),
            connector: ConnectorConfig {
                tls,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn auto_bare_host_defaults_https() {
        let ep = Connector::endpoint_tls(&cfg("x", TlsMode::Auto), "host.local:50051", None, None)
            .await
            .unwrap();
        assert_eq!(ep.uri().scheme_str(), Some("https"));
    }

    #[test]
    fn endpoints_implicit_from_server_url() {
        let c = cfg("https://lan:50052", TlsMode::Require);
        let eps = Connector::endpoints(&c);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].url, "https://lan:50052");
        assert!(eps[0].pki);
    }

    #[test]
    fn endpoints_explicit_list_wins_and_keeps_order() {
        let mut c = cfg("https://lan:50052", TlsMode::Require);
        c.connector.endpoints = vec![
            ConnectorEndpoint {
                url: "https://lan:50052".into(),
                pki: true,
            },
            ConnectorEndpoint {
                url: "https://grpc.example.com:443".into(),
                pki: false,
            },
        ];
        let eps = Connector::endpoints(&c);
        assert_eq!(eps.len(), 3);
        assert_eq!(eps[1].url, "https://grpc.example.com:443");
        assert!(!eps[1].pki);
        // :443 normalizes away — still an edge-like port-less host, so the
        // automatic wss:// shadow trails at lowest priority.
        assert_eq!(eps[2].url, "wss://grpc.example.com:443");
        // server_url ignored when endpoints are configured.
        assert_eq!(eps[0].url, "https://lan:50052");
    }

    #[test]
    fn endpoints_empty_when_nothing_configured() {
        let c = cfg("", TlsMode::Auto);
        assert!(Connector::endpoints(&c).is_empty());
    }

    #[test]
    fn primary_server_url_prefers_explicit_then_first_endpoint() {
        let mut c = cfg("", TlsMode::Auto);
        c.connector.endpoints = vec![
            ConnectorEndpoint {
                url: "https://lan:50052".into(),
                pki: true,
            },
            ConnectorEndpoint {
                url: "https://grpc.example.com".into(),
                pki: false,
            },
        ];
        assert_eq!(c.primary_server_url(), "https://lan:50052");
        c.server_url = "https://legacy:50052".into();
        assert_eq!(c.primary_server_url(), "https://legacy:50052");
    }

    #[tokio::test]
    async fn auto_keeps_explicit_http() {
        let ep = Connector::endpoint_tls(&cfg("x", TlsMode::Auto), "http://host:50051", None, None)
            .await
            .unwrap();
        assert_eq!(ep.uri().scheme_str(), Some("http"));
    }

    #[tokio::test]
    async fn require_upgrades_http_to_https() {
        let ep =
            Connector::endpoint_tls(&cfg("x", TlsMode::Require), "http://host:50051", None, None)
                .await
                .unwrap();
        assert_eq!(ep.uri().scheme_str(), Some("https"));
    }

    #[tokio::test]
    async fn off_forces_plaintext() {
        let ep = Connector::endpoint_tls(&cfg("x", TlsMode::Off), "https://host:50051", None, None)
            .await
            .unwrap();
        assert_eq!(ep.uri().scheme_str(), Some("http"));
    }

    #[tokio::test]
    async fn missing_ca_cert_path_fails_loudly() {
        let mut c = cfg("host:50051", TlsMode::Require);
        c.connector.ca_cert_path = "/nonexistent/ca.pem".into();
        assert!(Connector::endpoint_tls(&c, "host:50051", None, None)
            .await
            .is_err());
    }

    #[test]
    fn json_has_output_detects_content() {
        use serde_json::json;
        assert!(!super::json_has_output(
            &json!({"choices":[{"message":{"role":"","content":""}}]})
        ));
        assert!(super::json_has_output(
            &json!({"choices":[{"message":{"content":"hi"}}]})
        ));
        assert!(super::json_has_output(
            &json!({"choices":[{"delta":{"content":"x"}}]})
        ));
        assert!(super::json_has_output(
            &json!({"choices":[{"delta":{"tool_calls":[{"f":1}]}}]})
        ));
        assert!(super::json_has_output(
            &json!({"choices":[{"delta":{"reasoning_content":"r"}}]})
        ));
        assert!(super::json_has_output(&json!({"choices":[{"text":"abc"}]})));
        assert!(super::json_has_output(
            &json!({"data":[{"embedding":[0.1]}]})
        ));
        assert!(!super::json_has_output(&json!({"data":[]})));
        assert!(super::json_has_output(&json!({"text":"transcribed"})));
        assert!(super::json_has_output(&json!({"response":"ollama native"})));
    }

    #[test]
    fn bootstrap_url_maps_agent_ports_to_50053() {
        assert_eq!(
            derive_bootstrap_url("https://lan:50052"),
            "https://lan:50053"
        );
        assert_eq!(
            derive_bootstrap_url("https://lan:50051"),
            "https://lan:50053"
        );
        assert_eq!(
            derive_bootstrap_url("http://10.0.0.5:50054"),
            "http://10.0.0.5:50053"
        );
        assert_eq!(
            derive_bootstrap_url("https://lan:50053"),
            "https://lan:50053"
        );
    }

    #[test]
    fn bootstrap_url_leaves_public_and_custom_ports_alone() {
        // Port-less edge hostnames must not grow a phantom :50053 — the
        // edge only serves 443.
        assert_eq!(
            derive_bootstrap_url("https://grpc.ccl-lab.de"),
            "https://grpc.ccl-lab.de"
        );
        assert_eq!(
            derive_bootstrap_url("https://grpc.example.com:443"),
            "https://grpc.example.com:443"
        );
        // Custom gateway/agent ports have no bootstrap convention.
        assert_eq!(
            derive_bootstrap_url("https://host:8443"),
            "https://host:8443"
        );
        assert_eq!(
            derive_bootstrap_url("wss://grpc.ccl-lab.de"),
            "wss://grpc.ccl-lab.de"
        );
    }

    #[test]
    fn portless_https_endpoint_gains_wss_shadow() {
        let c = cfg("https://grpc.example.com", TlsMode::Require);
        let eps = Connector::endpoints(&c);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].url, "https://grpc.example.com");
        assert_eq!(eps[1].url, "wss://grpc.example.com");
        assert!(eps[1].pki);
    }

    #[test]
    fn no_wss_shadow_for_explicit_ports_or_plain_http() {
        let c = cfg("https://lan:50052", TlsMode::Require);
        assert_eq!(Connector::endpoints(&c).len(), 1);
        // :443 normalizes away — treated as edge-like, gains a shadow.
        let c = cfg("https://host:443", TlsMode::Require);
        let eps = Connector::endpoints(&c);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[1].url, "wss://host:443");
        let c = cfg("http://lan:50054", TlsMode::Require);
        assert_eq!(Connector::endpoints(&c).len(), 1);
    }

    #[test]
    fn wss_shadow_not_duplicated_when_explicitly_configured() {
        let mut c = cfg("", TlsMode::Auto);
        c.connector.endpoints = vec![
            ConnectorEndpoint {
                url: "https://grpc.example.com".into(),
                pki: true,
            },
            ConnectorEndpoint {
                url: "wss://grpc.example.com".into(),
                pki: false,
            },
        ];
        let eps = Connector::endpoints(&c);
        assert_eq!(eps.len(), 2);
        assert!(!eps[1].pki);
    }

    #[test]
    fn empty_generation_flags_zombie_only() {
        use loom_core::openai::Usage;
        let zero = Usage::default();
        // Zombie: 200, no output, zero tokens -> crash-classified failure.
        let e = super::empty_generation_failure(true, 200, false, &zero);
        assert!(matches!(e, Some(AdapterError::Io(_))));
        // Real output -> never flagged.
        assert!(super::empty_generation_failure(true, 200, true, &zero).is_none());
        // Upstream error statuses handled elsewhere.
        assert!(super::empty_generation_failure(true, 500, false, &zero).is_none());
        // Empty content but tokens were consumed -> legit generation.
        let used = Usage {
            prompt_tokens: 10,
            ..Default::default()
        };
        assert!(super::empty_generation_failure(true, 200, false, &used).is_none());
        // Non-text apis (binary audio) are exempt.
        assert!(super::empty_generation_failure(false, 200, false, &zero).is_none());
    }
}

#[cfg(test)]
mod endpoint_hint_tests {
    use super::*;

    fn ep(url: &str, pki: bool) -> ConnectorEndpoint {
        ConnectorEndpoint {
            url: url.into(),
            pki,
        }
    }
    fn hint(url: &str, pki: bool, scope: &str) -> EndpointHint {
        EndpointHint {
            url: url.into(),
            pki,
            scope: scope.into(),
        }
    }

    #[test]
    fn lan_hint_leads_public_config() {
        let out = merge_endpoints(
            &[ep("https://grpc.example.com", false)],
            &[hint("https://ts.internal:50052", true, "lan")],
        );
        assert_eq!(out[0].url, "https://ts.internal:50052");
        assert_eq!(out[1].url, "https://grpc.example.com");
    }

    #[test]
    fn public_hint_trails() {
        let out = merge_endpoints(
            &[ep("https://lan:50052", true)],
            &[hint("https://grpc.example.com", false, "public")],
        );
        assert_eq!(out[0].url, "https://lan:50052");
        assert_eq!(out[1].url, "https://grpc.example.com");
    }

    #[test]
    fn configured_url_keeps_own_pki_flag() {
        // Same URL hinted with a different pki flag: the configured entry
        // wins, only its position is promoted by the lan hint.
        let out = merge_endpoints(
            &[
                ep("https://grpc.example.com", false),
                ep("https://lan:50052", true),
            ],
            &[hint("https://lan:50052/", false, "lan")],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://lan:50052");
        assert!(out[0].pki);
    }

    #[test]
    fn no_hints_no_change() {
        let cfgd = vec![ep("https://a", true)];
        assert_eq!(merge_endpoints(&cfgd, &[]), cfgd);
    }

    #[test]
    fn url_host_parses() {
        assert_eq!(url_host("https://x.ccl-lab.de:50052/p"), "x.ccl-lab.de");
        assert_eq!(url_host("https://[::1]:50052"), "::1");
        assert_eq!(url_host("host:50052"), "host");
    }
}
