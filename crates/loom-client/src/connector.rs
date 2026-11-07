//! Connector: persistent client-initiated gRPC channel to the server.
//!
//! - Control stream (`Control`): hello/register, heartbeats, capacity/inventory
//!   reports, config push/ack, AssignRequest/Ack, CancelRequest.
//! - Per-request data streams (`RequestData`): opened by the client after
//!   accepting an assignment; request payload flows server->client, response
//!   bytes flow client->server.
//! - Reconnect with exponential backoff + session recovery + config full-sync.

use crate::backend_manager::BackendManager;
use crate::config::ConfigStore;
use crate::scheduler::{LocalScheduler, RequestClass};
use crate::telemetry;
use loom_core::capability::ApiType;
use loom_core::config::{ClientConfig, DesiredClientConfig, TlsMode};
use loom_protocol::limits;
use loom_protocol::pb::agent_client::AgentClient;
use loom_protocol::pb::*;
use loom_protocol::{client_msgs, frames, server_msgs};
use futures_util::StreamExt;
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
    pub capacity_notify: Notify,
    ctrl_tx: RwLock<Option<mpsc::Sender<ClientEnvelope>>>,
    agent: Mutex<Option<AgentClient<Channel>>>,
    active_requests: RwLock<HashMap<String, ActiveRequest>>,
    shutdown: CancellationToken,
}

impl Connector {
    pub fn new(
        cfg_store: Arc<ConfigStore>,
        backends: BackendManager,
        scheduler: LocalScheduler,
    ) -> Self {
        Self {
            state: Arc::new(ConnectorState {
                cfg_store,
                backends,
                scheduler,
                connected: AtomicBool::new(false),
                session_id: RwLock::new(String::new()),
                server_version_hint: RwLock::new(String::new()),
                capacity_notify: Notify::new(),
                ctrl_tx: RwLock::new(None),
                agent: Mutex::new(None),
                active_requests: RwLock::new(HashMap::new()),
                shutdown: CancellationToken::new(),
            }),
        }
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

    /// Abort a tier-2 request by request_id (admin/local cancel).
    pub fn abort_request(&self, request_id: &str) -> bool {
        if let Some(r) = self
            .state
            .active_requests
            .read()
            .unwrap()
            .get(request_id)
        {
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
        // Debounced capacity-report task.
        let state = self.state.clone();
        tokio::spawn(async move {
            loop {
                state.capacity_notify.notified().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                state.send_capacity_report().await;
            }
        });

        loop {
            if self.state.shutdown.is_cancelled() {
                return;
            }
            let cfg = self.state.cfg_store.get().await;
            if cfg.server_url.is_empty() {
                tokio::select! {
                    _ = self.state.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                }
            }
            match self.session(&cfg).await {
                Ok(()) => {
                    info!("control session ended cleanly");
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!(error = %e, "control session failed");
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
            self.set_disconnected().await;
            tokio::select! {
                _ = self.state.shutdown.cancelled() => return,
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

    async fn endpoint(cfg: &ClientConfig) -> Result<Endpoint, tonic::transport::Error> {
        let mut url = cfg.server_url.clone();
        if !url.contains("://") {
            url = format!("https://{url}");
        }
        let mut ep = Endpoint::from_shared(url)?;
        ep = ep
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(3600))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(20))
            .keep_alive_while_idle(true);
        let require_tls = cfg.connector.tls == TlsMode::Require
            || (cfg.connector.tls == TlsMode::Auto && ep.uri().scheme_str() == Some("https"));
        if require_tls && ep.uri().scheme_str() != Some("https") {
            // Endpoint was built from http; rebuild as https is handled by URL.
        }
        if ep.uri().scheme_str() == Some("https") {
            let mut tls = ClientTlsConfig::new().with_enabled_roots();
            if !cfg.connector.ca_cert_path.is_empty() {
                if let Ok(pem) = std::fs::read(&cfg.connector.ca_cert_path) {
                    tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
                }
            }
            ep = ep.tls_config(tls)?;
        }
        Ok(ep)
    }

    async fn session(&self, cfg: &ClientConfig) -> anyhow::Result<()> {
        let ep = Self::endpoint(cfg).await?;
        let channel = ep.connect().await?;
        let mut agent = AgentClient::new(channel);
        *self.state.agent.lock().await = Some(agent.clone());

        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ClientEnvelope>(64);
        let response = agent.control(ReceiverStream::new(ctrl_rx)).await?;
        let mut inbound = response.into_inner();

        // Hello
        let hello = self.build_hello(cfg).await;
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

        // Persist assigned client id / device credential on first enroll.
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

        *self.state.session_id.write().unwrap() = ack.session_id.clone();
        *self.state.ctrl_tx.write().unwrap() = Some(ctrl_tx.clone());
        self.state.connected.store(true, Ordering::SeqCst);
        telemetry::server_connected(true);

        // Config sync: if server's version differs, pull full snapshot.
        let local_version = self.state.cfg_store.get().await.config_version;
        if ack.config_version != local_version {
            debug!(local = local_version, remote = ack.config_version, "config version gap, pulling snapshot");
            let cred = self.state.cfg_store.get().await.device_credential;
            let cid = self.state.cfg_store.get().await.client_id;
            match agent
                .get_config(ConfigRequest {
                    client_id: cid,
                    credential: cred,
                    last_config_version: local_version,
                })
                .await
            {
                Ok(resp) => {
                    let snap = resp.into_inner();
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

        // Heartbeat task.
        let hb_secs = ack.heartbeat_interval_secs.max(2) as u64;
        let hb_tx = ctrl_tx.clone();
        let session_id = ack.session_id.clone();
        let sched = self.state.scheduler.clone();
        let cancel = self.state.shutdown.clone();
        let hb_task = tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(hb_secs));
            loop {
                t.tick().await;
                if cancel.is_cancelled() {
                    break;
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

        // Control message loop.
        let res = self
            .control_loop(&mut inbound, &ctrl_tx, &agent)
            .await;
        hb_task.abort();
        res
    }

    async fn build_hello(&self, cfg: &ClientConfig) -> Hello {
        let inv = self.build_inventory().await;
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
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
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

    async fn control_loop(
        &self,
        inbound: &mut tonic::Streaming<ServerEnvelope>,
        ctrl_tx: &mpsc::Sender<ClientEnvelope>,
        agent: &AgentClient<Channel>,
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
                    self.state.abort_request_fenced(&c.request_id, c.fencing_token);
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
            self.state.scheduler.update_limits(
                dep_id,
                dd.tier2_max,
                dd.local_reserve,
                dd.sharing_mode,
                dd.enabled,
            );
        }
        self.notify_capacity();
        Ok(())
    }

    fn handle_assign(
        &self,
        a: AssignRequest,
        ctrl_tx: mpsc::Sender<ClientEnvelope>,
        agent: AgentClient<Channel>,
    ) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let request_id = a.request_id.clone();
            // Admission check.
            let admit = state
                .scheduler
                .try_admit(&a.deployment_id, RequestClass::Tier2, &request_id);

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
                    let retryable = matches!(
                        r,
                        crate::scheduler::RejectReason::NoCapacity
                            | crate::scheduler::RejectReason::AdmissionClosed
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
            let _ = tx.send(client_msgs::capacity_report(caps)).await;
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
    }
}

/// Execute one assigned inference request over a dedicated data stream.
async fn run_request(
    state: Arc<ConnectorState>,
    mut agent: AgentClient<Channel>,
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

    let result = async {
        let (data_tx, data_rx) = mpsc::channel::<DataFrame>(32);
        let resp = agent
            .request_data(ReceiverStream::new(data_rx))
            .await
            .map_err(|e| format!("open data stream: {e}"))?;
        let mut inbound = resp.into_inner();

        // Head frame.
        data_tx
            .send(frames::head(&request_id, &lease_id, fencing))
            .await
            .map_err(|_| "send head".to_string())?;

        // Assemble request payload from request_json frames.
        let mut payload = Vec::with_capacity(a.payload_bytes as usize);
        let payload_deadline =
            tokio::time::Instant::now() + Duration::from_secs(30);
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

        let body: serde_json::Value =
            serde_json::from_slice(&payload).map_err(|e| format!("bad payload json: {e}"))?;
        let api: ApiType = a.api_type.parse().unwrap_or(ApiType::Chat);

        // Forward to backend.
        let resp = state
            .backends
            .forward(&a.deployment_id, api, body, cancel.clone())
            .await
            .map_err(|e| format!("upstream: {e}"))?;

        let mut ttft_ms: i64 = 0;
        let mut usage = loom_core::openai::Usage::default();
        let mut upstream_err: Option<String> = None;
        let mut finish_reason = String::new();
        let mut stream = resp.stream;

        // Bridge upstream bytes -> response_chunk frames, until End.
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = data_tx.send(frames::end(true, "cancelled")).await;
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
                    // Opportunistically parse usage/finish from SSE tail.
                    inspect_sse_tail(&bytes, &mut usage, &mut finish_reason);
                    if data_tx
                        .send(frames::response_chunk(bytes.to_vec()))
                        .await
                        .is_err()
                    {
                        return Err("data stream closed".to_string());
                    }
                }
                Some(Err(e)) => {
                    upstream_err = Some(e.to_string());
                    break;
                }
                None => break,
            }
        }

        let status = if cancel.is_cancelled() {
            "cancelled"
        } else if upstream_err.is_some() {
            "failed"
        } else {
            "done"
        };
        let _ = data_tx
            .send(frames::end(status == "cancelled", &finish_reason))
            .await;
        Ok::<_, String>((status.to_string(), upstream_err, usage, ttft_ms))
    }
    .await;

    let (status, err, usage, ttft_ms) = match result {
        Ok(v) => v,
        Err(e) => {
            if e == "cancelled" {
                ("cancelled".to_string(), Some(e), loom_core::openai::Usage::default(), 0)
            } else {
                ("failed".to_string(), Some(e), loom_core::openai::Usage::default(), 0)
            }
        }
    };


    let _ = ctrl_tx
        .send(client_msgs::request_end(
            &request_id,
            &lease_id,
            &status,
            &err.unwrap_or_default(),
            usage.prompt_tokens,
            usage.completion_tokens,
            ttft_ms,
            started.elapsed().as_millis() as i64,
        ))
        .await;

    telemetry::req_finished(&status);
    telemetry::req_duration(started.elapsed().as_secs_f64(), "tier2");
    state.scheduler.finish_request(&request_id);
    state.active_requests.write().unwrap().remove(&request_id);
    state.notify_capacity_now();
    drop(token);
}

/// Parse trailing SSE data for usage + finish_reason (best effort).
fn inspect_sse_tail(
    bytes: &bytes::Bytes,
    usage: &mut loom_core::openai::Usage,
    finish_reason: &mut String,
) {
    let Ok(text) = std::str::from_utf8(bytes) else { return };
    for line in text.split("\n\n") {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else { continue };
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
    }
}
